//! Persistence for combos: named, ordered provider/model fallback chains.

use sqlx::{Row, SqlitePool, sqlite::SqliteRow};

use crate::error::AppError;

/// Maximum length of a combo name.
pub const MAX_NAME_LENGTH: usize = 64;

/// Maximum length of a step model identifier (after trimming).
pub const MAX_MODEL_LENGTH: usize = 256;

/// A named fallback combo.
pub struct Combo {
    pub id: String,
    pub name: String,
}

/// One entry in a combo's ordered fallback chain.
pub struct ComboStep {
    pub id: String,
    pub combo_id: String,
    pub position: i64,
    pub provider_id: String,
    pub model: String,
}

/// Validate a combo name: non-empty, at most [`MAX_NAME_LENGTH`]
/// characters, restricted to ASCII letters, digits, `-`, `_`, and `.`.
fn validate_name(name: &str) -> Result<(), AppError> {
    if name.is_empty() {
        return Err(AppError::InvalidRequest(
            "combo name must not be empty".into(),
        ));
    }
    if name.chars().count() > MAX_NAME_LENGTH {
        return Err(AppError::InvalidRequest(format!(
            "combo name must be at most {MAX_NAME_LENGTH} characters"
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(AppError::InvalidRequest(
            "combo name may only contain ASCII letters, digits, dashes, underscores, and dots"
                .into(),
        ));
    }
    Ok(())
}

/// Validate a step model identifier, returning the trimmed value.
fn validate_model(model: &str) -> Result<String, AppError> {
    let model = model.trim();
    if model.is_empty() {
        return Err(AppError::InvalidRequest(
            "step model must not be empty".into(),
        ));
    }
    if model.chars().count() > MAX_MODEL_LENGTH {
        return Err(AppError::InvalidRequest(format!(
            "step model must be at most {MAX_MODEL_LENGTH} characters"
        )));
    }
    Ok(model.to_owned())
}

/// Begin a write transaction using `BEGIN IMMEDIATE` so the write lock is
/// acquired before any reads, keeping concurrent combo mutations serialized.
async fn begin_immediate(
    pool: &SqlitePool,
) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, AppError> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
}

async fn touch_combo(connection: &mut sqlx::SqliteConnection, id: &str) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE combos SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
    )
    .bind(id)
    .execute(connection)
    .await?;
    Ok(())
}

fn combo_from_row(row: &SqliteRow) -> Result<Combo, AppError> {
    Ok(Combo {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
    })
}

fn step_from_row(row: &SqliteRow) -> Result<ComboStep, AppError> {
    Ok(ComboStep {
        id: row.try_get("id")?,
        combo_id: row.try_get("combo_id")?,
        position: row.try_get("position")?,
        provider_id: row.try_get("provider_id")?,
        model: row.try_get("model")?,
    })
}

/// All combos, ordered by name.
pub async fn list(pool: &SqlitePool) -> Result<Vec<Combo>, AppError> {
    let rows = sqlx::query("SELECT id, name FROM combos ORDER BY name")
        .fetch_all(pool)
        .await?;
    rows.iter().map(combo_from_row).collect()
}

/// Look up a combo by id.
pub async fn get(pool: &SqlitePool, id: &str) -> Result<Combo, AppError> {
    let row = sqlx::query("SELECT id, name FROM combos WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref()
        .map(combo_from_row)
        .transpose()?
        .ok_or_else(|| AppError::NotFound(format!("combo '{id}' was not found")))
}

/// Look up a combo by its unique name.
pub async fn by_name(pool: &SqlitePool, name: &str) -> Result<Combo, AppError> {
    let row = sqlx::query("SELECT id, name FROM combos WHERE name = ?")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    row.as_ref()
        .map(combo_from_row)
        .transpose()?
        .ok_or_else(|| AppError::NotFound(format!("no combo named '{name}' exists")))
}

/// Create a combo with a fresh UUID id.
pub async fn create(pool: &SqlitePool, name: &str) -> Result<Combo, AppError> {
    validate_name(name)?;
    let combo = Combo {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_owned(),
    };
    let result = sqlx::query("INSERT INTO combos (id, name) VALUES (?1, ?2)")
        .bind(&combo.id)
        .bind(&combo.name)
        .execute(pool)
        .await;
    match result {
        Ok(_) => Ok(combo),
        Err(error) if super::is_constraint_violation(&error) => Err(AppError::InvalidRequest(
            format!("a combo named '{name}' already exists"),
        )),
        Err(error) => Err(error.into()),
    }
}

/// Rename a combo.
pub async fn rename(pool: &SqlitePool, id: &str, name: &str) -> Result<(), AppError> {
    validate_name(name)?;
    let result = sqlx::query("UPDATE combos SET name = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?")
        .bind(name)
        .bind(id)
        .execute(pool)
        .await;
    match result {
        Ok(result) if result.rows_affected() == 0 => {
            Err(AppError::NotFound(format!("combo '{id}' was not found")))
        }
        Ok(_) => Ok(()),
        Err(error) if super::is_constraint_violation(&error) => Err(AppError::InvalidRequest(
            format!("a combo named '{name}' already exists"),
        )),
        Err(error) => Err(error.into()),
    }
}

/// Delete a combo by id; its steps are removed by the schema's
/// `ON DELETE CASCADE`.
pub async fn delete(pool: &SqlitePool, id: &str) -> Result<(), AppError> {
    let result = sqlx::query("DELETE FROM combos WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await;
    match result {
        Ok(result) if result.rows_affected() == 0 => {
            Err(AppError::NotFound(format!("combo '{id}' was not found")))
        }
        Ok(_) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// The steps of a combo in execution order (ascending position).
pub async fn steps(pool: &SqlitePool, combo_id: &str) -> Result<Vec<ComboStep>, AppError> {
    let rows = sqlx::query(
        "SELECT id, combo_id, position, provider_id, model
         FROM combo_steps
         WHERE combo_id = ?
         ORDER BY position",
    )
    .bind(combo_id)
    .fetch_all(pool)
    .await?;
    rows.iter().map(step_from_row).collect()
}

/// Append a step to the end of a combo.
///
/// The combo and provider must both exist; otherwise the insert would
/// violate a foreign key and fail with [`AppError::InvalidRequest`].
pub async fn add_step(
    pool: &SqlitePool,
    combo_id: &str,
    provider_id: &str,
    model: &str,
) -> Result<(), AppError> {
    let model = validate_model(model)?;
    let mut transaction = begin_immediate(pool).await?;

    let combo_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM combos WHERE id = ?")
        .bind(combo_id)
        .fetch_one(&mut *transaction)
        .await?;
    if combo_count == 0 {
        return Err(AppError::InvalidRequest(format!(
            "combo '{combo_id}' does not exist"
        )));
    }

    let provider_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM providers WHERE id = ?")
        .bind(provider_id)
        .fetch_one(&mut *transaction)
        .await?;
    if provider_count == 0 {
        return Err(AppError::InvalidRequest(format!(
            "provider '{provider_id}' does not exist"
        )));
    }

    let max_position: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MAX(position) FROM combo_steps WHERE combo_id = ?",
    )
    .bind(combo_id)
    .fetch_one(&mut *transaction)
    .await?;
    let position = max_position.map_or(0, |max| max + 1);

    let result = sqlx::query(
        "INSERT INTO combo_steps (id, combo_id, position, provider_id, model)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(combo_id)
    .bind(position)
    .bind(provider_id)
    .bind(&model)
    .execute(&mut *transaction)
    .await;

    match result {
        Ok(_) => {
            touch_combo(&mut transaction, combo_id).await?;
            transaction.commit().await?;
            Ok(())
        }
        Err(error) if super::is_constraint_violation(&error) => Err(AppError::InvalidRequest(
            "step could not be added: its combo or provider no longer exists".into(),
        )),
        Err(error) => Err(error.into()),
    }
}

/// Remove a step and transactionally renumber the remaining steps so
/// positions stay contiguous starting at zero.
pub async fn remove_step(pool: &SqlitePool, combo_id: &str, step_id: &str) -> Result<(), AppError> {
    let mut transaction = begin_immediate(pool).await?;

    let result = sqlx::query("DELETE FROM combo_steps WHERE id = ? AND combo_id = ?")
        .bind(step_id)
        .bind(combo_id)
        .execute(&mut *transaction)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!(
            "step '{step_id}' was not found in combo '{combo_id}'"
        )));
    }

    // Renumber ascending: each target index is at most the row's current
    // position, so the UNIQUE (combo_id, position) constraint can never
    // conflict mid-pass.
    let rows = sqlx::query("SELECT id FROM combo_steps WHERE combo_id = ? ORDER BY position")
        .bind(combo_id)
        .fetch_all(&mut *transaction)
        .await?;
    for (index, row) in rows.iter().enumerate() {
        let id: String = row.get("id");
        let position = index as i64;
        sqlx::query("UPDATE combo_steps SET position = ? WHERE id = ? AND position <> ?")
            .bind(position)
            .bind(&id)
            .bind(position)
            .execute(&mut *transaction)
            .await?;
    }

    touch_combo(&mut transaction, combo_id).await?;
    transaction.commit().await?;
    Ok(())
}

/// Move a step one slot up (`direction = -1`) or down (`direction = 1`).
///
/// Moving past either end of the combo is a no-op. Swaps park the displaced
/// step on a temporary negative position first so the swap cannot violate
/// the UNIQUE (combo_id, position) constraint.
pub async fn move_step(
    pool: &SqlitePool,
    combo_id: &str,
    step_id: &str,
    direction: i64,
) -> Result<(), AppError> {
    if direction != -1 && direction != 1 {
        return Err(AppError::InvalidRequest(
            "direction must be -1 (up) or 1 (down)".into(),
        ));
    }

    let mut transaction = begin_immediate(pool).await?;

    let row = sqlx::query("SELECT position FROM combo_steps WHERE id = ? AND combo_id = ?")
        .bind(step_id)
        .bind(combo_id)
        .fetch_optional(&mut *transaction)
        .await?;
    let Some(row) = row else {
        return Err(AppError::NotFound(format!(
            "step '{step_id}' was not found in combo '{combo_id}'"
        )));
    };
    let position: i64 = row.try_get("position")?;

    let target = position + direction;
    let at_boundary = if direction < 0 {
        target < 0
    } else {
        // Nothing sits below this step: moving down is a no-op.
        let steps_after: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM combo_steps WHERE combo_id = ? AND position > ?",
        )
        .bind(combo_id)
        .bind(position)
        .fetch_one(&mut *transaction)
        .await?;
        steps_after == 0
    };

    if !at_boundary {
        let occupant =
            sqlx::query("SELECT id FROM combo_steps WHERE combo_id = ? AND position = ?")
                .bind(combo_id)
                .bind(target)
                .fetch_optional(&mut *transaction)
                .await?;
        match occupant {
            Some(occupant) => {
                let occupant_id: String = occupant.try_get("id")?;
                // Park the displaced step on a temporary position below the
                // valid range while the two rows trade places.
                sqlx::query("UPDATE combo_steps SET position = -1 WHERE id = ?")
                    .bind(&occupant_id)
                    .execute(&mut *transaction)
                    .await?;
                sqlx::query("UPDATE combo_steps SET position = ? WHERE id = ?")
                    .bind(target)
                    .bind(step_id)
                    .execute(&mut *transaction)
                    .await?;
                sqlx::query("UPDATE combo_steps SET position = ? WHERE id = ?")
                    .bind(position)
                    .bind(&occupant_id)
                    .execute(&mut *transaction)
                    .await?;
            }
            None => {
                // Empty target slot (an interior gap): simply take it.
                sqlx::query("UPDATE combo_steps SET position = ? WHERE id = ?")
                    .bind(target)
                    .bind(step_id)
                    .execute(&mut *transaction)
                    .await?;
            }
        }
    }

    touch_combo(&mut transaction, combo_id).await?;
    transaction.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::providers::{self as provider_db, Provider};
    use crate::providers::Protocol;

    async fn pool_with_providers() -> SqlitePool {
        let pool = crate::db::connect("sqlite::memory:")
            .await
            .expect("connect");
        for (id, name) in [("provider-a", "Alpha"), ("provider-b", "Beta")] {
            provider_db::save(
                &pool,
                &Provider {
                    id: id.into(),
                    name: name.into(),
                    base_url: "https://api.example.invalid".into(),
                    protocol: Protocol::OpenAi,
                    encrypted_api_key: vec![1, 2, 3],
                    enabled: true,
                },
            )
            .await
            .expect("save provider");
        }
        pool
    }

    async fn model_sequence(pool: &SqlitePool, combo_id: &str) -> Vec<String> {
        steps(pool, combo_id)
            .await
            .expect("steps")
            .into_iter()
            .map(|step| step.model)
            .collect()
    }

    async fn step_id_at(pool: &SqlitePool, combo_id: &str, index: usize) -> String {
        steps(pool, combo_id).await.expect("steps")[index]
            .id
            .clone()
    }

    #[tokio::test]
    async fn steps_keep_insertion_order_with_contiguous_positions() {
        let pool = pool_with_providers().await;
        let combo = create(&pool, "fallback").await.expect("combo");
        for model in ["first", "second", "third"] {
            add_step(&pool, &combo.id, "provider-a", model)
                .await
                .expect("add step");
        }

        let stored = steps(&pool, &combo.id).await.expect("steps");
        let models: Vec<_> = stored.iter().map(|step| step.model.as_str()).collect();
        assert_eq!(models, ["first", "second", "third"]);
        let positions: Vec<_> = stored.iter().map(|step| step.position).collect();
        assert_eq!(positions, [0, 1, 2]);
    }

    #[tokio::test]
    async fn moving_steps_swaps_neighbors_and_ignores_boundaries() {
        let pool = pool_with_providers().await;
        let combo = create(&pool, "moves").await.expect("combo");
        for model in ["a", "b", "c"] {
            add_step(&pool, &combo.id, "provider-a", model)
                .await
                .expect("add step");
        }

        // Moving the first step up and the last step down are no-ops.
        let first = step_id_at(&pool, &combo.id, 0).await;
        let last = step_id_at(&pool, &combo.id, 2).await;
        move_step(&pool, &combo.id, &first, -1).await.expect("move");
        move_step(&pool, &combo.id, &last, 1).await.expect("move");
        assert_eq!(model_sequence(&pool, &combo.id).await, ["a", "b", "c"]);

        // Moving the middle step up swaps it with the first.
        let middle = step_id_at(&pool, &combo.id, 1).await;
        move_step(&pool, &combo.id, &middle, -1)
            .await
            .expect("move");
        assert_eq!(model_sequence(&pool, &combo.id).await, ["b", "a", "c"]);

        // The last step still cannot move down.
        move_step(&pool, &combo.id, &last, 1).await.expect("move");
        assert_eq!(model_sequence(&pool, &combo.id).await, ["b", "a", "c"]);

        // Moving the new first step down swaps it back.
        let first = step_id_at(&pool, &combo.id, 0).await;
        move_step(&pool, &combo.id, &first, 1).await.expect("move");
        assert_eq!(model_sequence(&pool, &combo.id).await, ["a", "b", "c"]);

        // Positions remain contiguous and ordered after all moves.
        let stored = steps(&pool, &combo.id).await.expect("steps");
        let positions: Vec<_> = stored.iter().map(|step| step.position).collect();
        assert_eq!(positions, [0, 1, 2]);
    }

    #[tokio::test]
    async fn removing_a_step_renumbers_the_remainder() {
        let pool = pool_with_providers().await;
        let combo = create(&pool, "prune").await.expect("combo");
        for model in ["a", "b", "c"] {
            add_step(&pool, &combo.id, "provider-b", model)
                .await
                .expect("add step");
        }

        let middle = step_id_at(&pool, &combo.id, 1).await;
        remove_step(&pool, &combo.id, &middle)
            .await
            .expect("remove");

        let stored = steps(&pool, &combo.id).await.expect("steps");
        let models: Vec<_> = stored.iter().map(|step| step.model.as_str()).collect();
        assert_eq!(models, ["a", "c"]);
        let positions: Vec<_> = stored.iter().map(|step| step.position).collect();
        assert_eq!(positions, [0, 1]);

        // Removing an unknown step reports NotFound.
        assert!(matches!(
            remove_step(&pool, &combo.id, "missing-step").await,
            Err(AppError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn steps_require_existing_combo_and_provider() {
        let pool = pool_with_providers().await;
        let combo = create(&pool, "keys").await.expect("combo");

        assert!(matches!(
            add_step(&pool, "missing-combo", "provider-a", "gpt").await,
            Err(AppError::InvalidRequest(_))
        ));
        assert!(matches!(
            add_step(&pool, &combo.id, "missing-provider", "gpt").await,
            Err(AppError::InvalidRequest(_))
        ));
    }

    #[tokio::test]
    async fn provider_delete_is_restricted_while_referenced() {
        let pool = pool_with_providers().await;
        let combo = create(&pool, "locked").await.expect("combo");
        add_step(&pool, &combo.id, "provider-a", "gpt")
            .await
            .expect("add step");
        assert!(matches!(
            provider_db::delete(&pool, "provider-a").await,
            Err(AppError::InvalidRequest(_))
        ));
    }

    #[tokio::test]
    async fn combo_delete_cascades_its_steps() {
        let pool = pool_with_providers().await;
        let combo = create(&pool, "doomed").await.expect("combo");
        add_step(&pool, &combo.id, "provider-a", "gpt")
            .await
            .expect("add step");

        delete(&pool, &combo.id).await.expect("delete");
        assert!(steps(&pool, &combo.id).await.expect("steps").is_empty());
        assert!(matches!(
            get(&pool, &combo.id).await,
            Err(AppError::NotFound(_))
        ));

        // With the combo (and its cascaded step) gone, the provider can be
        // deleted again.
        provider_db::delete(&pool, "provider-a")
            .await
            .expect("provider delete");
    }

    #[tokio::test]
    async fn invalid_combo_names_are_rejected() {
        let pool = pool_with_providers().await;
        let long = "x".repeat(65);
        for name in ["", "has space", "slash/name", "ünicode", long.as_str()] {
            assert!(
                matches!(create(&pool, name).await, Err(AppError::InvalidRequest(_))),
                "expected {name:?} to be rejected"
            );
        }

        // 64 characters are allowed, and renaming validates identically.
        let combo = create(&pool, &"y".repeat(64)).await.expect("create");
        assert!(matches!(
            rename(&pool, &combo.id, "no spaces").await,
            Err(AppError::InvalidRequest(_))
        ));
    }

    #[tokio::test]
    async fn duplicate_combo_names_are_rejected() {
        let pool = pool_with_providers().await;
        create(&pool, "solo").await.expect("create");
        assert!(matches!(
            create(&pool, "solo").await,
            Err(AppError::InvalidRequest(_))
        ));

        let other = create(&pool, "other").await.expect("create");
        assert!(matches!(
            rename(&pool, &other.id, "solo").await,
            Err(AppError::InvalidRequest(_))
        ));
    }

    #[tokio::test]
    async fn invalid_step_models_are_rejected() {
        let pool = pool_with_providers().await;
        let combo = create(&pool, "models").await.expect("combo");

        for model in ["", "   "] {
            assert!(matches!(
                add_step(&pool, &combo.id, "provider-a", model).await,
                Err(AppError::InvalidRequest(_))
            ));
        }
        let long = "m".repeat(257);
        assert!(matches!(
            add_step(&pool, &combo.id, "provider-a", &long).await,
            Err(AppError::InvalidRequest(_))
        ));

        let max = "m".repeat(256);
        add_step(&pool, &combo.id, "provider-a", &max)
            .await
            .expect("256 characters are allowed");
        add_step(&pool, &combo.id, "provider-a", "  trimmed  ")
            .await
            .expect("add step");

        let stored = steps(&pool, &combo.id).await.expect("steps");
        assert_eq!(stored[1].model, "trimmed");
    }

    #[tokio::test]
    async fn move_step_validates_direction_and_missing_references() {
        let pool = pool_with_providers().await;
        let combo = create(&pool, "direction").await.expect("combo");
        add_step(&pool, &combo.id, "provider-a", "m")
            .await
            .expect("add step");
        let step = step_id_at(&pool, &combo.id, 0).await;

        for direction in [0, 2, -2] {
            assert!(matches!(
                move_step(&pool, &combo.id, &step, direction).await,
                Err(AppError::InvalidRequest(_))
            ));
        }
        assert!(matches!(
            move_step(&pool, &combo.id, "missing-step", 1).await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            move_step(&pool, "missing-combo", &step, 1).await,
            Err(AppError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn combos_can_be_looked_up_renamed_and_deleted() {
        let pool = pool_with_providers().await;
        let combo = create(&pool, "lookup").await.expect("combo");

        let by_id = get(&pool, &combo.id).await.expect("by id");
        assert_eq!(by_id.name, "lookup");
        let named = by_name(&pool, "lookup").await.expect("by name");
        assert_eq!(named.id, combo.id);

        assert!(matches!(
            get(&pool, "missing").await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            by_name(&pool, "missing").await,
            Err(AppError::NotFound(_))
        ));

        rename(&pool, &combo.id, "renamed").await.expect("rename");
        assert_eq!(get(&pool, &combo.id).await.expect("get").name, "renamed");
        assert!(matches!(
            rename(&pool, "missing", "x").await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            delete(&pool, "missing").await,
            Err(AppError::NotFound(_))
        ));

        create(&pool, "aaa").await.expect("create");
        let names: Vec<_> = list(&pool)
            .await
            .expect("list")
            .into_iter()
            .map(|combo| combo.name)
            .collect();
        assert_eq!(names, ["aaa", "renamed"]);
    }
}
