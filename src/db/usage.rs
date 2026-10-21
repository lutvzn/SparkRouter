//! Usage over the retained history, not lifetime totals or an upstream invoice.
//! Each provider counts a request once, even when several of its keys are tried.
//! Only the selected attempt receives the request's reported usage and cost.

use crate::{db::request_history::OUTCOME_STREAMING, error::AppError};
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderTotals {
    pub slug: String,
    pub name: String,
    pub requests: i64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub usage_requests: i64,
    pub cost_known: i64,
}
/// Prompt-cache results for one combo, by the `combo/<name>` it was called as.
#[derive(Debug, Clone, PartialEq)]
pub struct ComboCache {
    pub name: String,
    /// The combo's current mode and timeout; `None` once it no longer exists.
    pub affinity_mode: Option<String>,
    pub affinity_minutes: Option<i64>,
    pub requests: i64,
    /// Requests whose usage reported cached tokens (zero included).
    pub reported: i64,
    /// Reported requests with at least one cached token.
    pub hits: i64,
    pub cached_tokens: Option<i64>,
    /// Input tokens of the reported requests only, so the share compares like with like.
    pub input_tokens: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overview {
    pub requests: i64,
    pub successes: i64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub input_known: i64,
    pub output_known: i64,
    pub cache_known: i64,
    pub cost_known: i64,
    pub providers: Vec<ProviderTotals>,
    pub combos: Vec<ComboCache>,
}

pub async fn overview(pool: &SqlitePool) -> Result<Overview, AppError> {
    let mut tx = pool.begin().await?;
    let r = sqlx::query(
        "SELECT COUNT(*) AS requests,
                COALESCE(SUM(outcome = 'success'), 0) AS successes,
                SUM(input_tokens) AS input_tokens,
                SUM(output_tokens) AS output_tokens,
                SUM(cached_tokens) AS cached_tokens,
                SUM(cost_usd) AS cost_usd,
                COUNT(input_tokens) AS input_known,
                COUNT(output_tokens) AS output_known,
                COUNT(cached_tokens) AS cache_known,
                COUNT(cost_usd) AS cost_known
         FROM request_history WHERE outcome <> ?",
    )
    .bind(OUTCOME_STREAMING)
    .fetch_one(&mut *tx)
    .await?;
    let summary = Overview {
        requests: r.try_get("requests")?,
        successes: r.try_get("successes")?,
        input_tokens: r.try_get("input_tokens")?,
        output_tokens: r.try_get("output_tokens")?,
        cached_tokens: r.try_get("cached_tokens")?,
        cost_usd: r.try_get("cost_usd")?,
        input_known: r.try_get("input_known")?,
        output_known: r.try_get("output_known")?,
        cache_known: r.try_get("cache_known")?,
        cost_known: r.try_get("cost_known")?,
        providers: Vec::new(),
        combos: Vec::new(),
    };
    let rows = sqlx::query(
        "WITH finished AS (
             SELECT * FROM request_history WHERE outcome <> ?
         ), ranked_attempts AS (
             SELECT a.request_id,
                    COALESCE(a.provider_slug, 'connection-' || a.provider_id) AS slug,
                    COALESCE(a.provider_group_name, a.provider_name) AS name,
                    a.id AS last_row,
                    ROW_NUMBER() OVER (
                        PARTITION BY a.request_id, COALESCE(a.provider_slug, 'connection-' || a.provider_id)
                        ORDER BY a.ordinal DESC
                    ) AS rank
             FROM request_attempts a JOIN finished f ON f.id = a.request_id
         ), ns AS (
             SELECT * FROM ranked_attempts WHERE rank = 1
         ), selected AS (
             SELECT a.request_id,
                    COALESCE(a.provider_slug, 'connection-' || a.provider_id) AS slug,
                    f.input_tokens, f.output_tokens, f.cached_tokens, f.cost_usd
             FROM request_attempts a JOIN finished f ON f.id = a.request_id
             WHERE a.outcome = 'selected' AND a.ordinal = (
                 SELECT MAX(x.ordinal) FROM request_attempts x
                 WHERE x.request_id = a.request_id AND x.outcome = 'selected'
             )
         ), totals AS (
             SELECT ns.slug,
                    (SELECT n.name FROM ns n WHERE n.slug = ns.slug
                     ORDER BY n.last_row DESC LIMIT 1) AS name,
                    COUNT(*) AS requests,
                    SUM(s.input_tokens) AS input_tokens,
                    SUM(s.output_tokens) AS output_tokens,
                    SUM(s.cached_tokens) AS cached_tokens,
                    SUM(s.cost_usd) AS cost_usd,
                    SUM(CASE WHEN s.input_tokens IS NOT NULL OR s.output_tokens IS NOT NULL
                             THEN 1 ELSE 0 END) AS usage_requests,
                    COUNT(s.cost_usd) AS cost_known
             FROM ns LEFT JOIN selected s ON s.request_id = ns.request_id AND s.slug = ns.slug
             GROUP BY ns.slug
         )
         SELECT * FROM totals
         ORDER BY requests DESC, (COALESCE(input_tokens, 0) + COALESCE(output_tokens, 0)) DESC,
                  slug ASC LIMIT 10",
    )
        .bind(OUTCOME_STREAMING).fetch_all(&mut *tx).await?;
    let providers = rows
        .into_iter()
        .map(|r| {
            Ok(ProviderTotals {
                slug: r.try_get("slug")?,
                name: r.try_get("name")?,
                requests: r.try_get("requests")?,
                input_tokens: r.try_get("input_tokens")?,
                output_tokens: r.try_get("output_tokens")?,
                cached_tokens: r.try_get("cached_tokens")?,
                cost_usd: r.try_get("cost_usd")?,
                usage_requests: r.try_get("usage_requests")?,
                cost_known: r.try_get("cost_known")?,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    let combos = sqlx::query(
        "SELECT substr(h.model, 7) AS name,
                MAX(c.affinity_mode) AS affinity_mode,
                MAX(c.sticky_minutes) AS affinity_minutes,
                COUNT(*) AS requests,
                COUNT(h.cached_tokens) AS reported,
                COALESCE(SUM(h.cached_tokens > 0), 0) AS hits,
                SUM(h.cached_tokens) AS cached_tokens,
                SUM(CASE WHEN h.cached_tokens IS NOT NULL THEN h.input_tokens END) AS input_tokens
         FROM request_history h
         LEFT JOIN combos c ON c.name = substr(h.model, 7)
         WHERE h.outcome <> ? AND substr(h.model, 1, 6) = 'combo/'
         GROUP BY substr(h.model, 7)
         ORDER BY requests DESC, name ASC LIMIT 10",
    )
    .bind(OUTCOME_STREAMING)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|r| {
        Ok(ComboCache {
            name: r.try_get("name")?,
            affinity_mode: r.try_get("affinity_mode")?,
            affinity_minutes: r.try_get("affinity_minutes")?,
            requests: r.try_get("requests")?,
            reported: r.try_get("reported")?,
            hits: r.try_get("hits")?,
            cached_tokens: r.try_get("cached_tokens")?,
            input_tokens: r.try_get("input_tokens")?,
        })
    })
    .collect::<Result<Vec<_>, AppError>>()?;
    tx.commit().await?;
    Ok(Overview {
        providers,
        combos,
        ..summary
    })
}
