//! Dashboard pages for scoped, optionally expiring gateway API keys.

use askama::Template;
use axum::{
    extract::{Form, Path, Query, State},
    response::{Html, Response},
};
use secrecy::ExposeSecret;
use serde::Deserialize;

use super::{CsrfForm, check_csrf, render, see_other};
use crate::app::AppState;
use crate::db::api_keys::{BudgetPeriod, Expiry, KeyAccess, KeyBudget, KeyLimits};
use crate::error::AppError;

#[derive(Template)]
#[template(path = "api_keys/list.html")]
struct ApiKeysPage {
    app_name: &'static str,
    csrf_token: String,
    keys: Vec<KeyRow>,
    available_models: Vec<String>,
}

struct KeyRow {
    id: String,
    name: String,
    prefix: String,
    created_at: String,
    restricted: bool,
    models: String,
    expiry: String,
    expired: bool,
    rpm: String,
    concurrency: String,
    active: u32,
    rejected: u64,
    /// `day`, `month`, or empty without a budget.
    budget_period: String,
    token_budget: String,
    cost_budget: String,
    /// The budget and this period's usage, ready to display.
    budget_summary: String,
}

#[derive(Template)]
#[template(path = "api_keys/created.html")]
struct CreatedPage {
    app_name: &'static str,
    name: String,
    secret: String,
    revealed: bool,
    regenerated: bool,
}

#[derive(Deserialize)]
pub(crate) struct RevealQuery {
    regenerated: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct CreateForm {
    _csrf: String,
    name: String,
    #[serde(flatten)]
    policy: PolicyForm,
}

#[derive(Deserialize)]
pub(crate) struct PolicyForm {
    #[serde(default)]
    access: String,
    #[serde(default)]
    models: String,
    #[serde(default)]
    expiry: String,
    #[serde(default)]
    expires_days: String,
    requests_per_minute: Option<String>,
    max_concurrent: Option<String>,
    /// `none`, `day`, or `month`; absent keeps an existing budget.
    budget_period: Option<String>,
    token_budget: Option<String>,
    cost_budget_usd: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct UpdateForm {
    _csrf: String,
    #[serde(flatten)]
    policy: PolicyForm,
}

struct Policy {
    access: KeyAccess,
    expiry: Expiry,
    limits: Option<KeyLimits>,
    /// `None` keeps the current budget, `Some(None)` removes it.
    budget: Option<Option<KeyBudget>>,
}

fn budget(form: &PolicyForm) -> Result<Option<Option<KeyBudget>>, AppError> {
    let filled = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let tokens = filled(&form.token_budget);
    let cost = filled(&form.cost_budget_usd);
    let period = match form.budget_period.as_deref().map(str::trim) {
        None => return Ok(None),
        Some("" | "none") if tokens.is_none() && cost.is_none() => return Ok(Some(None)),
        Some("" | "none") => {
            return Err(AppError::InvalidRequest(
                "Choose a daily or monthly period for the budget".into(),
            ));
        }
        Some(period) => BudgetPeriod::parse(period)?,
    };
    let invalid = || {
        AppError::InvalidRequest(
            "Budgets need a whole number of tokens and a USD amount such as 2.50".into(),
        )
    };
    Ok(Some(Some(
        KeyBudget {
            period,
            tokens: tokens
                .map(|value| value.parse().map_err(|_| invalid()))
                .transpose()?,
            cost_usd: cost
                .map(|value| value.parse().map_err(|_| invalid()))
                .transpose()?,
        }
        .validate()?,
    )))
}

fn policy(form: PolicyForm, editing: bool) -> Result<Policy, AppError> {
    let budget = budget(&form)?;
    fn number(value: Option<&str>) -> Result<Option<u32>, AppError> {
        match value.map(str::trim).filter(|s| !s.is_empty()) {
            None => Ok(None),
            Some(value) => value.parse().map(Some).map_err(|_| {
                AppError::InvalidRequest("Limits must be positive whole numbers or blank".into())
            }),
        }
    }
    let limits = if form.requests_per_minute.is_none() && form.max_concurrent.is_none() {
        None
    } else {
        Some(
            KeyLimits {
                requests_per_minute: number(form.requests_per_minute.as_deref())?,
                max_concurrent: number(form.max_concurrent.as_deref())?,
            }
            .validate()?,
        )
    };
    // Never silently discard submitted scopes on an unrestricted key.
    let access = match form.access.as_str() {
        "" if !editing && form.models.trim().is_empty() => KeyAccess::All,
        "all" if form.models.trim().is_empty() => KeyAccess::All,
        "selected" => {
            if form.models.len() > 210_000 {
                return Err(AppError::InvalidRequest(
                    "Allowed model list is too large".into(),
                ));
            }
            KeyAccess::Selected(
                form.models
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect(),
            )
        }
        _ => {
            return Err(AppError::InvalidRequest(
                "Choose selected access when providing allowed model IDs".into(),
            ));
        }
    };
    let expiry =
        match form.expiry.as_str() {
            "keep" if editing && form.expires_days.trim().is_empty() => Expiry::Keep,
            "" if editing && form.expires_days.trim().is_empty() => Expiry::Keep,
            "" | "never" if form.expires_days.trim().is_empty() => Expiry::Never,
            "days" => Expiry::AfterDays(form.expires_days.trim().parse().map_err(|_| {
                AppError::InvalidRequest("Expiry must be 1–3650 days from now".into())
            })?),
            _ => {
                return Err(AppError::InvalidRequest(
                    "Choose a valid expiry option".into(),
                ));
            }
        };
    Ok(Policy {
        access,
        expiry,
        limits,
        budget,
    })
}

pub(crate) async fn list(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let now = crate::app::unix_now();
    let spending = crate::db::api_keys::spending(&state.db).await?;
    let mut keys = Vec::new();
    for key in crate::db::api_keys::list(&state.db).await? {
        // The form's budget fields, and the budget with this period's usage.
        let (budget_period, token_budget, cost_budget, budget_summary) = match key.budget {
            None => Default::default(),
            Some(budget) => {
                let (tokens, cost) = spending
                    .get(&(key.id.clone(), budget.period.label(now)))
                    .copied()
                    .unwrap_or_default();
                let mut limits = Vec::new();
                if let Some(max) = budget.tokens {
                    limits.push(format!("{} tokens", super::home::number(max)));
                }
                if let Some(max) = budget.cost_usd {
                    limits.push(super::home::optional_cost(Some(max)));
                }
                let summary = format!(
                    "{} per UTC {} · used this period: {} tokens, {}",
                    limits.join(" or "),
                    budget.period.as_str(),
                    super::home::number(tokens),
                    super::home::optional_cost(Some(cost)),
                );
                let text = |value: Option<String>| value.unwrap_or_default();
                (
                    budget.period.as_str().to_owned(),
                    text(budget.tokens.map(|n| n.to_string())),
                    text(budget.cost_usd.map(|n| n.to_string())),
                    summary,
                )
            }
        };
        let (restricted, models) = match key.access {
            KeyAccess::All => (false, String::new()),
            KeyAccess::Selected(models) => (true, models.join("\n")),
        };
        let stats = state.key_limits.snapshot(&key.id);
        keys.push(KeyRow {
            budget_period,
            token_budget,
            cost_budget,
            budget_summary,
            rpm: key
                .limits
                .requests_per_minute
                .map(|n| n.to_string())
                .unwrap_or_default(),
            concurrency: key
                .limits
                .max_concurrent
                .map(|n| n.to_string())
                .unwrap_or_default(),
            active: stats.active,
            rejected: stats.rejected,
            id: key.id,
            name: key.name,
            prefix: key.prefix,
            created_at: key.created_at,
            restricted,
            models,
            expired: key.expired,
            expiry: key.expires_at.unwrap_or_else(|| "Never".into()),
        });
    }
    let mut available_models: Vec<String> = crate::db::combos::list(&state.db)
        .await?
        .into_iter()
        .map(|combo| format!("combo/{}", combo.name))
        .collect();
    available_models.extend(crate::db::connection_models::routable_model_ids(&state.db).await?);
    render(ApiKeysPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
        keys,
        available_models,
    })
}

pub(crate) async fn create(
    State(state): State<AppState>,
    Form(form): Form<CreateForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let policy = policy(form.policy, false)?;
    let issued = crate::db::api_keys::create_with_budget(
        &state.db,
        &state.crypto,
        &form.name,
        policy.access,
        policy.expiry,
        policy.limits.unwrap_or_default(),
        policy.budget.flatten(),
    )
    .await?;
    Ok(see_other(&format!("/api-keys/{}", issued.id)))
}

pub(crate) async fn update_policy(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<UpdateForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let policy = policy(form.policy, true)?;
    crate::db::api_keys::update_policy_with_budget(
        &state.db,
        &id,
        policy.access,
        policy.expiry,
        policy.limits,
        policy.budget,
    )
    .await?;
    Ok(see_other("/api-keys"))
}

pub(crate) async fn regenerate(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let issued = crate::db::api_keys::regenerate(&state.db, &state.crypto, &id).await?;
    Ok(see_other(&format!("/api-keys/{}?regenerated=1", issued.id)))
}

pub(crate) async fn created(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<RevealQuery>,
) -> Result<Html<String>, AppError> {
    let secret = crate::db::api_keys::take_reveal(&state.db, &state.crypto, &id).await?;
    let key = crate::db::api_keys::list(&state.db)
        .await?
        .into_iter()
        .find(|key| key.id == id)
        .ok_or_else(|| AppError::NotFound(format!("API key '{id}' was not found")))?;
    render(CreatedPage {
        app_name: crate::APP_NAME,
        name: key.name,
        secret: secret
            .as_ref()
            .map(|value| value.expose_secret().to_owned())
            .unwrap_or_default(),
        revealed: secret.is_some(),
        regenerated: query.regenerated.is_some(),
    })
}

pub(crate) async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    crate::db::api_keys::delete(&state.db, &id).await?;
    Ok(see_other("/api-keys"))
}
