//! Provider admin pages and actions.
//!
//! The dashboard groups stored providers by endpoint: one tile per catalog
//! preset (always visible, in catalog order) plus one tile per persisted
//! custom provider group. A custom group owns its display name, base URL,
//! and protocol, and holds any number of named connections (each with its
//! own API key); preset pages list the unassigned rows whose endpoint
//! matches the preset exactly.

use std::collections::HashMap;

use askama::Template;
use axum::{
    extract::{Form, Path, State},
    response::{Html, Response},
};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use super::{CsrfForm, check_csrf, provider_catalog, render, see_other};
use crate::app::AppState;
use crate::crypto::Crypto;
use crate::db::provider_groups::{self, CustomGroup, GroupSummary};
use crate::db::providers::Provider;
use crate::error::AppError;
use crate::providers::Protocol;

const NAME_MAX_CHARS: usize = 100;

#[derive(Template)]
#[template(path = "providers/list.html")]
pub(crate) struct ProvidersPage {
    app_name: &'static str,
    tiles: Vec<ProviderTile>,
}

/// One square tile on the providers page: a catalog preset or a persisted
/// custom provider group linking to its group page.
pub(crate) struct ProviderTile {
    name: String,
    protocol: String,
    href: String,
    connection_count: usize,
    enabled_count: usize,
    /// False for catalog presets, true for persisted custom groups (which
    /// carry the Custom tag and can be deleted from their group page).
    is_custom: bool,
}

#[derive(Template)]
#[template(path = "providers/group.html")]
pub(crate) struct ProviderGroupPage {
    app_name: &'static str,
    csrf_token: String,
    name: String,
    base_url: String,
    protocol: String,
    action: String,
    providers: Vec<ProviderRow>,
    /// False for catalog preset pages, true for persisted custom groups
    /// (which show the Custom badge and the group delete form).
    is_custom: bool,
}

/// One named connection on a group page. Endpoint fields live on the page
/// itself because every row in a group shares them by construction.
pub(crate) struct ProviderRow {
    id: String,
    name: String,
    enabled: bool,
    has_key: bool,
}

impl ProviderRow {
    fn from_stored(provider: Provider) -> Self {
        Self {
            has_key: !provider.encrypted_api_key.is_empty(),
            id: provider.id,
            name: provider.name,
            enabled: provider.enabled,
        }
    }
}

#[derive(Template)]
#[template(path = "providers/form.html")]
pub(crate) struct ProviderFormPage {
    app_name: &'static str,
    csrf_token: String,
    is_edit: bool,
    action: String,
    name: String,
    base_url: String,
    enabled: bool,
    protocols: Vec<ProtocolOption>,
    protocol: String,
    key_placeholder: &'static str,
    /// Grouped connections cannot change their endpoint here: the editable
    /// controls are replaced by read-only metadata plus hidden form fields.
    endpoint_locked: bool,
    /// Where the Cancel link returns to (the group page when grouped).
    back_href: String,
}

pub(crate) struct ProtocolOption {
    value: String,
    label: &'static str,
    selected: bool,
}

/// Custom provider creation form: name, base URL, and protocol only. The
/// group starts empty; connections with their own API keys are added on the
/// group page afterwards.
#[derive(Template)]
#[template(path = "providers/custom.html")]
pub(crate) struct CustomProviderFormPage {
    app_name: &'static str,
    csrf_token: String,
}

#[derive(Template)]
#[template(path = "providers/_test_result.html")]
pub(crate) struct TestResultFragment {
    ok: bool,
    message: String,
}

#[derive(Deserialize)]
pub(crate) struct ProviderForm {
    _csrf: String,
    name: String,
    base_url: String,
    protocol: String,
    /// Checkbox: present (any non-empty value) when checked, absent when not.
    enabled: Option<String>,
    #[serde(deserialize_with = "deserialize_api_key")]
    api_key: SecretString,
}

/// Form behind `/providers/custom`: creates a persisted custom provider
/// group (no connection yet, so no API-key or enabled fields).
#[derive(Deserialize)]
pub(crate) struct CustomProviderForm {
    _csrf: String,
    name: String,
    base_url: String,
    protocol: String,
}

fn deserialize_api_key<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SecretString, D::Error> {
    String::deserialize(deserializer).map(SecretString::from)
}

/// Custom Askama filters for the provider templates. Askama resolves
/// `{{ value|first_char }}` in templates declared in this module to
/// [`filters::first_char`].
mod filters {
    /// The uppercased first character of a tile name, for the square avatar.
    /// Empty names render an empty avatar; the output is auto-escaped.
    pub fn first_char<S: std::fmt::Display>(
        source: S,
        _values: &dyn askama::Values,
    ) -> Result<String, askama::Error> {
        Ok(source
            .to_string()
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_default())
    }
}

pub(crate) async fn list(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let stored = crate::db::providers::list(&state.db).await?;
    let memberships = provider_groups::connection_group_names(&state.db).await?;
    let group_summaries = provider_groups::list(&state.db).await?;
    render(ProvidersPage {
        app_name: crate::APP_NAME,
        tiles: build_tiles(stored, &memberships, group_summaries),
    })
}

/// Group page for a catalog preset: unassigned connections whose endpoint
/// matches the preset exactly, plus an Add connection form pinned to that
/// endpoint.
pub(crate) async fn catalog(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Html<String>, AppError> {
    let preset = provider_catalog::find(&slug)
        .ok_or_else(|| AppError::NotFound(format!("provider preset '{slug}' was not found")))?;
    let stored = crate::db::providers::list(&state.db).await?;
    let memberships = provider_groups::connection_group_names(&state.db).await?;
    let providers = group_rows(stored, &memberships, preset.base_url, &preset.protocol);
    render(ProviderGroupPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
        name: preset.name.to_string(),
        base_url: preset.base_url.to_string(),
        protocol: preset.protocol.as_str().to_string(),
        action: format!("/providers/catalog/{slug}"),
        providers,
        is_custom: false,
    })
}

/// Group page for a persisted custom provider group, showing the group's
/// own name and endpoint metadata with its member connections.
pub(crate) async fn group(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Html<String>, AppError> {
    let group = resolve_group(&state, &id).await?;
    let providers = provider_groups::members(&state.db, &group.id)
        .await?
        .into_iter()
        .map(ProviderRow::from_stored)
        .collect();
    render(ProviderGroupPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
        name: group.name,
        base_url: group.base_url,
        protocol: group.protocol.as_str().to_string(),
        action: format!("/providers/groups/{}", group.id),
        providers,
        is_custom: true,
    })
}

/// Shows the custom provider form (name, base URL, protocol). No API key or
/// enabled flag here: the group is created empty and connections are added
/// on the group page.
pub(crate) async fn new(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    render(CustomProviderFormPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
    })
}

/// Legacy create endpoint, kept with its original form fields and redirect
/// for existing clients and tests. Preset endpoints stay flat provider rows;
/// every other endpoint is folded into a persisted custom group that matches
/// the endpoint and protocol exactly (creating one when none exists).
pub(crate) async fn create(
    State(state): State<AppState>,
    Form(form): Form<ProviderForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let provider = new_provider_from_form(&form, &state.crypto)?;
    let url = group_url(&provider.base_url);
    if provider_catalog::matching(&url, &provider.protocol).is_some() {
        crate::db::providers::save(&state.db, &provider).await?;
    } else {
        provider_groups::save_legacy_connection(
            &state.db,
            &provider,
            &provider_catalog::display_name(&url, &provider.protocol),
        )
        .await?;
    }
    Ok(see_other("/providers"))
}

/// Creates an empty persisted custom provider group, then redirects to its
/// group page where the first connection can be added.
pub(crate) async fn custom_create(
    State(state): State<AppState>,
    Form(form): Form<CustomProviderForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let name = validate_name(&form.name)?;
    let base_url = crate::providers::normalize_base_url(form.base_url.trim())?;
    let protocol = parse_protocol(&form.protocol)?;
    let group = provider_groups::create(&state.db, name, &base_url, &protocol).await?;
    Ok(see_other(&format!("/providers/groups/{}", group.id)))
}

/// Creates a connection for a catalog preset. The endpoint comes from the
/// preset, never from the submitted form, so a crafted form cannot attach a
/// connection to a different group.
pub(crate) async fn catalog_create(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Form(mut form): Form<ProviderForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let preset = provider_catalog::find(&slug)
        .ok_or_else(|| AppError::NotFound(format!("provider preset '{slug}' was not found")))?;
    form.base_url = preset.base_url.to_string();
    form.protocol = preset.protocol.as_str().to_string();
    let provider = new_provider_from_form(&form, &state.crypto)?;
    crate::db::providers::save(&state.db, &provider).await?;
    Ok(see_other(&format!("/providers/catalog/{slug}")))
}

/// Creates a connection in a persisted custom group. The endpoint is copied
/// from the group, ignoring whatever the form submitted, so a crafted form
/// cannot attach a connection to a different endpoint.
pub(crate) async fn group_create(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(mut form): Form<ProviderForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let group = resolve_group(&state, &id).await?;
    form.base_url = group.base_url;
    form.protocol = group.protocol.as_str().to_string();
    let provider = new_provider_from_form(&form, &state.crypto)?;
    provider_groups::create_connection(&state.db, &group.id, &provider).await?;
    Ok(see_other(&format!("/providers/groups/{}", group.id)))
}

pub(crate) async fn edit(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Html<String>, AppError> {
    let provider = crate::db::providers::get(&state.db, &id).await?;
    // Grouped connections inherit their provider's endpoint: show it
    // read-only here and link back to the provider page.
    let group = provider_groups::by_connection(&state.db, &id).await?;
    let (endpoint_locked, back_href) = match &group {
        Some(group) => (true, format!("/providers/groups/{}", group.id)),
        None => (false, "/providers".to_string()),
    };
    render(ProviderFormPage {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.as_str().to_owned(),
        is_edit: true,
        action: format!("/providers/{id}"),
        name: provider.name,
        base_url: provider.base_url,
        enabled: provider.enabled,
        protocols: protocol_options(Some(&provider.protocol)),
        protocol: provider.protocol.as_str().to_string(),
        key_placeholder: "Stored securely; leave blank to keep",
        endpoint_locked,
        back_href,
    })
}

pub(crate) async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(mut form): Form<ProviderForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    let existing = crate::db::providers::get(&state.db, &id).await?;
    // A grouped connection's endpoint belongs to its group; override the
    // form so the row cannot be moved to a different endpoint.
    let group = provider_groups::by_connection(&state.db, &id).await?;
    if let Some(group) = &group {
        form.base_url = group.base_url.clone();
        form.protocol = group.protocol.as_str().to_string();
    }
    let provider = apply_provider_form(existing, &form, &state.crypto)?;
    if group.is_none()
        && provider_catalog::matching(&provider.base_url, &provider.protocol).is_none()
    {
        let name = provider_catalog::display_name(&provider.base_url, &provider.protocol);
        provider_groups::save_legacy_connection(&state.db, &provider, &name).await?;
    } else {
        crate::db::providers::save(&state.db, &provider).await?;
    }
    Ok(see_other("/providers"))
}

pub(crate) async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    crate::db::providers::delete(&state.db, &id).await?;
    Ok(see_other("/providers"))
}

/// Deletes a persisted custom provider group and, atomically, every
/// connection that belongs to it. Only the canonical group id is accepted:
/// unlike the group pages, the legacy connection-id aliases are deliberately
/// not resolved for this destructive action, and the database layer rejects
/// any id that is not a group (catalog slugs included) with a 404.
pub(crate) async fn delete_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    check_csrf(&state, &form._csrf)?;
    provider_groups::delete(&state.db, &id).await?;
    Ok(see_other("/providers"))
}

/// Runs a connection test against the provider and returns a small HTML
/// fragment for HTMX to swap next to the Test button. Failures render as a 200
/// fragment (HTMX does not swap error responses by default).
pub(crate) async fn test(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<Html<String>, AppError> {
    check_csrf(&state, &form._csrf)?;
    let provider = crate::db::providers::get(&state.db, &id).await?;
    let fragment =
        match crate::providers::test_connection(&state.client, &state.crypto, &provider).await {
            Ok(()) => TestResultFragment {
                ok: true,
                message: "reachable".to_string(),
            },
            // AppError display strings are deliberately non-sensitive.
            Err(error) => TestResultFragment {
                ok: false,
                message: error.to_string(),
            },
        };
    render(fragment)
}

/// Resolves a custom group by its canonical id; ids that instead name a
/// member connection resolve to that connection's group, so pre-migration
/// tile links (`/providers/groups/{connection-id}`) keep working. Anything
/// else is a 404.
async fn resolve_group(state: &AppState, id: &str) -> Result<CustomGroup, AppError> {
    match provider_groups::get(&state.db, id).await {
        Ok(group) => Ok(group),
        Err(AppError::NotFound(_)) => provider_groups::by_connection(&state.db, id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("provider group '{id}' was not found"))),
        Err(error) => Err(error),
    }
}

/// Endpoint group key: the base URL with trailing slashes trimmed, so
/// `https://host/v1` and `https://host/v1/` are one endpoint.
fn group_url(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_string()
}

/// True when a stored connection belongs to the endpoint group identified by
/// `base_url` (already normalized) and `protocol`.
fn matches_endpoint(provider: &Provider, base_url: &str, protocol: &Protocol) -> bool {
    group_url(&provider.base_url) == base_url && provider.protocol == *protocol
}

/// Connection rows for a preset page: unassigned rows whose endpoint matches
/// the preset exactly. Rows owned by a custom group stay on their group's
/// page even when the endpoint matches the preset.
fn group_rows(
    stored: Vec<Provider>,
    memberships: &HashMap<String, String>,
    base_url: &str,
    protocol: &Protocol,
) -> Vec<ProviderRow> {
    stored
        .into_iter()
        .filter(|p| matches_endpoint(p, base_url, protocol) && !memberships.contains_key(&p.id))
        .map(ProviderRow::from_stored)
        .collect()
}

/// Position of a preset in [`provider_catalog::PRESETS`], used to index the
/// per-preset counters on the providers page.
fn preset_index(preset: &provider_catalog::Preset) -> Option<usize> {
    provider_catalog::PRESETS
        .iter()
        .position(|candidate| candidate.slug == preset.slug)
}

/// Builds the providers page tiles: every catalog preset in order (counting
/// only unassigned rows that match the preset endpoint), then one tile per
/// persisted custom group — including groups with no connections yet —
/// linking to the canonical group id.
fn build_tiles(
    stored: Vec<Provider>,
    memberships: &HashMap<String, String>,
    group_summaries: Vec<GroupSummary>,
) -> Vec<ProviderTile> {
    let mut preset_counts = vec![(0usize, 0usize); provider_catalog::PRESETS.len()];
    for provider in &stored {
        if memberships.contains_key(&provider.id) {
            continue;
        }
        let url = group_url(&provider.base_url);
        let matched = provider_catalog::matching(&url, &provider.protocol);
        if let Some(index) = matched.and_then(preset_index) {
            preset_counts[index].0 += 1;
            preset_counts[index].1 += usize::from(provider.enabled);
        }
    }

    let mut tiles: Vec<ProviderTile> = provider_catalog::PRESETS
        .iter()
        .zip(preset_counts)
        .map(|(preset, (connection_count, enabled_count))| ProviderTile {
            name: preset.name.to_string(),
            protocol: preset.protocol.as_str().to_string(),
            href: format!("/providers/catalog/{}", preset.slug),
            connection_count,
            enabled_count,
            is_custom: false,
        })
        .collect();

    tiles.extend(group_summaries.into_iter().map(|summary| ProviderTile {
        name: summary.group.name,
        protocol: summary.group.protocol.as_str().to_string(),
        href: format!("/providers/groups/{}", summary.group.id),
        connection_count: summary.connection_count,
        enabled_count: summary.enabled_count,
        is_custom: true,
    }));

    tiles
}

/// Builds a fresh provider (new UUID) from the form. A blank API key is allowed
/// so local no-auth upstreams can be registered.
fn new_provider_from_form(form: &ProviderForm, crypto: &Crypto) -> Result<Provider, AppError> {
    let provider = Provider {
        id: uuid::Uuid::new_v4().to_string(),
        name: String::new(),
        base_url: String::new(),
        protocol: Protocol::OpenAi,
        encrypted_api_key: Vec::new(),
        enabled: false,
    };
    apply_provider_form(provider, form, crypto)
}

/// Validates the form and merges it into `provider`. The API key is moved into
/// a `SecretString` immediately; when the submitted key is blank the existing
/// ciphertext is preserved byte-for-byte (never decrypted for display, never
/// re-encrypted, so the stored envelope does not change).
fn apply_provider_form(
    mut provider: Provider,
    form: &ProviderForm,
    crypto: &Crypto,
) -> Result<Provider, AppError> {
    let name = validate_name(&form.name)?;
    let base_url = crate::providers::normalize_base_url(form.base_url.trim())?;
    let protocol = parse_protocol(&form.protocol)?;
    let enabled = form.enabled.as_deref().is_some_and(|v| !v.is_empty());

    let api_key = form.api_key.expose_secret().trim();
    if !api_key.is_empty() {
        let secret = SecretString::from(api_key.to_string());
        provider.encrypted_api_key = crypto.encrypt_api_key(&provider.id, &secret)?;
    }

    provider.name = name.to_string();
    provider.base_url = base_url;
    provider.protocol = protocol;
    provider.enabled = enabled;
    Ok(provider)
}

/// Trims a submitted name and enforces the shared length limits. Returns the
/// trimmed name for storage.
fn validate_name(name: &str) -> Result<&str, AppError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::InvalidRequest("name is required".into()));
    }
    if name.chars().count() > NAME_MAX_CHARS {
        return Err(AppError::InvalidRequest(format!(
            "name must be at most {NAME_MAX_CHARS} characters"
        )));
    }
    Ok(name)
}

/// Parses a submitted protocol through `Protocol::FromStr` so form values
/// always round-trip regardless of the canonical spelling.
fn parse_protocol(value: &str) -> Result<Protocol, AppError> {
    value
        .trim()
        .parse::<Protocol>()
        .map_err(|_| AppError::InvalidRequest("unknown protocol".into()))
}

/// `<select>` options driven by `Protocol::as_str()` so form values always
/// round-trip through `FromStr` regardless of the canonical spelling.
fn protocol_options(selected: Option<&Protocol>) -> Vec<ProtocolOption> {
    let selected_value = selected.map(|protocol| protocol.as_str());
    [
        (Protocol::OpenAi.as_str(), "OpenAI"),
        (Protocol::Anthropic.as_str(), "Anthropic"),
    ]
    .into_iter()
    .map(|(value, label)| ProtocolOption {
        value: value.to_string(),
        label,
        selected: selected_value == Some(value),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use secrecy::ExposeSecret;

    fn test_crypto() -> Crypto {
        Crypto::from_base64(&SecretString::from(STANDARD.encode([9u8; 32]))).unwrap()
    }

    fn form(api_key: &str) -> ProviderForm {
        ProviderForm {
            _csrf: "irrelevant".to_string(),
            name: "  Renamed provider  ".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            protocol: Protocol::OpenAi.as_str().to_string(),
            enabled: Some("1".to_string()),
            api_key: SecretString::from(api_key),
        }
    }

    fn stored_provider(crypto: &Crypto, key: &str) -> Provider {
        let id = uuid::Uuid::new_v4().to_string();
        let encrypted_api_key = crypto
            .encrypt_api_key(&id, &SecretString::from(key.to_string()))
            .unwrap();
        Provider {
            id,
            name: "Old".to_string(),
            base_url: "https://old.example.com/v1".to_string(),
            protocol: Protocol::Anthropic,
            encrypted_api_key,
            enabled: false,
        }
    }

    fn connection(id: &str, base_url: &str, protocol: Protocol, enabled: bool) -> Provider {
        Provider {
            id: id.to_string(),
            name: format!("connection {id}"),
            base_url: base_url.to_string(),
            protocol,
            encrypted_api_key: Vec::new(),
            enabled,
        }
    }

    fn summary(
        id: &str,
        name: &str,
        base_url: &str,
        protocol: Protocol,
        connection_count: usize,
        enabled_count: usize,
    ) -> GroupSummary {
        GroupSummary {
            group: CustomGroup {
                id: id.to_string(),
                name: name.to_string(),
                base_url: base_url.to_string(),
                protocol,
            },
            connection_count,
            enabled_count,
        }
    }

    #[test]
    fn blank_api_key_edit_preserves_identical_ciphertext() {
        let crypto = test_crypto();
        let provider = stored_provider(&crypto, "sk-original-secret");
        let original_ciphertext = provider.encrypted_api_key.clone();

        let updated = apply_provider_form(provider, &form("   "), &crypto).unwrap();

        // Exact same encrypted bytes, not a re-encryption of the same secret.
        assert_eq!(updated.encrypted_api_key, original_ciphertext);
        assert_eq!(
            crypto
                .decrypt_api_key(&updated.id, &updated.encrypted_api_key)
                .unwrap()
                .expose_secret(),
            "sk-original-secret"
        );
        // Other fields were still updated.
        assert_eq!(updated.name, "Renamed provider");
        assert_eq!(updated.protocol.as_str(), Protocol::OpenAi.as_str());
        assert!(updated.enabled);
    }

    #[test]
    fn non_blank_api_key_edit_replaces_ciphertext() {
        let crypto = test_crypto();
        let provider = stored_provider(&crypto, "sk-original-secret");
        let original_ciphertext = provider.encrypted_api_key.clone();

        let updated = apply_provider_form(provider, &form("sk-new-secret"), &crypto).unwrap();

        assert_ne!(updated.encrypted_api_key, original_ciphertext);
        assert_eq!(
            crypto
                .decrypt_api_key(&updated.id, &updated.encrypted_api_key)
                .unwrap()
                .expose_secret(),
            "sk-new-secret"
        );
    }

    #[test]
    fn blank_api_key_create_stores_no_ciphertext() {
        let crypto = test_crypto();
        let created = new_provider_from_form(&form(""), &crypto).unwrap();
        assert!(created.encrypted_api_key.is_empty());
        assert_eq!(created.protocol.as_str(), Protocol::OpenAi.as_str());
        assert!(created.enabled);
        assert!(!created.id.is_empty());
    }

    #[test]
    fn rejects_blank_and_overlong_names() {
        let crypto = test_crypto();

        let provider = stored_provider(&crypto, "k");
        let mut invalid = form("");
        invalid.name = "   ".to_string();
        assert!(apply_provider_form(provider, &invalid, &crypto).is_err());

        let provider = stored_provider(&crypto, "k");
        let mut overlong = form("");
        overlong.name = "x".repeat(NAME_MAX_CHARS + 1);
        assert!(apply_provider_form(provider, &overlong, &crypto).is_err());

        let provider = stored_provider(&crypto, "k");
        let mut unknown = form("");
        unknown.protocol = "carrier-pigeon".to_string();
        assert!(apply_provider_form(provider, &unknown, &crypto).is_err());
    }

    #[test]
    fn tiles_reflect_persisted_groups_and_membership() {
        let preset = &provider_catalog::PRESETS[0];
        let stored = vec![
            // Unassigned preset row: counted on the preset tile.
            connection("p1", preset.base_url, preset.protocol, true),
            // Assigned row whose endpoint matches the preset: owned by its
            // custom group, so the preset tile must not count it.
            connection("p2", preset.base_url, preset.protocol, true),
        ];
        let mut memberships = HashMap::new();
        memberships.insert("p2".to_string(), "OpenAI at work".to_string());
        let groups = vec![
            // Empty groups still get a tile so the card is visible.
            summary(
                "custom-empty",
                "Empty <Group>",
                "https://empty.example/v1",
                Protocol::Anthropic,
                0,
                0,
            ),
            summary(
                "custom-work",
                "Work <LLM>",
                "https://shared.example/v1",
                Protocol::Anthropic,
                2,
                1,
            ),
            // Same endpoint as custom-work but a distinct named group: the
            // persisted groups are never merged by endpoint.
            summary(
                "custom-personal",
                "Personal LLM",
                "https://shared.example/v1",
                Protocol::Anthropic,
                1,
                0,
            ),
        ];

        let tiles = build_tiles(stored, &memberships, groups);

        assert_eq!(tiles.len(), provider_catalog::PRESETS.len() + 3);
        let first = &tiles[0];
        assert_eq!(first.href, format!("/providers/catalog/{}", preset.slug));
        assert!(!first.is_custom, "preset tiles are not custom");
        assert_eq!(first.connection_count, 1, "assigned row is excluded");
        assert_eq!(first.enabled_count, 1);
        for tile in &tiles[1..provider_catalog::PRESETS.len()] {
            assert_eq!(tile.connection_count, 0, "other presets stay untouched");
            assert!(!tile.is_custom);
        }
        let empty = &tiles[provider_catalog::PRESETS.len()];
        assert_eq!(empty.href, "/providers/groups/custom-empty");
        assert_eq!(empty.name, "Empty <Group>");
        assert_eq!(empty.protocol, Protocol::Anthropic.as_str());
        assert_eq!(empty.connection_count, 0);
        assert_eq!(empty.enabled_count, 0);
        assert!(empty.is_custom, "persisted groups are custom");
        let work = &tiles[provider_catalog::PRESETS.len() + 1];
        assert_eq!(work.name, "Work <LLM>");
        assert_eq!(work.href, "/providers/groups/custom-work");
        assert_eq!(work.connection_count, 2);
        assert_eq!(work.enabled_count, 1);
        assert!(work.is_custom);
        let personal = tiles.last().unwrap();
        assert_eq!(personal.href, "/providers/groups/custom-personal");
        assert_eq!(personal.name, "Personal LLM");
        assert_eq!(personal.connection_count, 1);
        assert_eq!(personal.enabled_count, 0);
        assert!(personal.is_custom);
    }

    #[test]
    fn pages_render_with_escaping_and_csrf() {
        use askama::Template;

        const CSRF: &str = "csrf-test-token";

        let page = ProvidersPage {
            app_name: crate::APP_NAME,
            tiles: vec![
                ProviderTile {
                    name: "Preset <A> & Co".to_string(),
                    protocol: "openai".to_string(),
                    href: "/providers/catalog/preset-a".to_string(),
                    connection_count: 2,
                    enabled_count: 1,
                    is_custom: false,
                },
                ProviderTile {
                    name: "Custom <Host>".to_string(),
                    protocol: "anthropic".to_string(),
                    href: "/providers/groups/g1".to_string(),
                    connection_count: 0,
                    enabled_count: 0,
                    is_custom: true,
                },
            ],
        }
        .render()
        .unwrap();
        // Auto-escaped user data (askama 0.14 emits numeric entities) and centralized title.
        assert!(page.contains("Preset &#60;A&#62; &#38; Co"));
        assert!(page.contains("<title>SparkRouter :: Providers</title>"));
        assert!(page.contains("href=\"/providers/catalog/preset-a\""));
        assert!(page.contains("href=\"/providers/groups/g1\""));
        // The always-present custom tile still links to the plain form.
        assert!(page.contains("href=\"/providers/new\""));
        assert!(page.contains("Custom provider"));
        assert!(page.contains("Create a provider, then add connections."));
        // Tiles are links only: no forms, so no CSRF inputs.
        assert!(!page.contains("name=\"_csrf\""));
        // Only the persisted custom-group tile carries the Custom tag; the
        // preset tile and the static create tile do not.
        assert_eq!(
            page.matches("<span class=\"badge\">Custom</span>").count(),
            1
        );

        let group = ProviderGroupPage {
            app_name: crate::APP_NAME,
            csrf_token: CSRF.to_string(),
            name: "Local <LLM> & Friends".to_string(),
            base_url: "http://127.0.0.1:8080/v1".to_string(),
            protocol: "openai".to_string(),
            action: "/providers/groups/custom-g1".to_string(),
            providers: vec![ProviderRow {
                id: "p1".to_string(),
                name: "Local <LLM> & Friends".to_string(),
                enabled: true,
                has_key: false,
            }],
            is_custom: true,
        }
        .render()
        .unwrap();
        assert!(group.contains("<title>SparkRouter :: Local &#60;LLM&#62; &#38; Friends</title>"));
        assert!(group.contains("Local &#60;LLM&#62; &#38; Friends"));
        assert!(group.contains("hx-post=\"/providers/p1/test\""));
        assert!(group.contains("action=\"/providers/p1/delete\""));
        // Custom groups show the Custom badge and a group delete form whose
        // action is the canonical group action, plus the deletion warning.
        assert!(group.contains("<span class=\"badge\">Custom</span>"));
        assert!(group.contains("action=\"/providers/groups/custom-g1/delete\""));
        assert!(group.contains("Delete provider"));
        assert!(group.contains(
            "Deleting this provider also deletes its connections. Remove any linked combo steps first."
        ));
        // Test + delete buttons, the Add connection form, and the group
        // delete form each carry CSRF.
        assert_eq!(group.matches("name=\"_csrf\"").count(), 4);
        // The add form posts back to the group and pins the endpoint.
        assert!(group.contains("action=\"/providers/groups/custom-g1\""));
        assert!(
            group.contains("type=\"hidden\" name=\"base_url\" value=\"http://127.0.0.1:8080/v1\"")
        );
        assert!(group.contains("type=\"hidden\" name=\"protocol\" value=\"openai\""));

        let catalog_page = ProviderGroupPage {
            app_name: crate::APP_NAME,
            csrf_token: CSRF.to_string(),
            name: "Preset <A> & Co".to_string(),
            base_url: "https://preset.example.com/v1".to_string(),
            protocol: "openai".to_string(),
            action: "/providers/catalog/preset-a".to_string(),
            providers: vec![ProviderRow {
                id: "p9".to_string(),
                name: "Preset <A> & Co".to_string(),
                enabled: true,
                has_key: true,
            }],
            is_custom: false,
        }
        .render()
        .unwrap();
        // Preset pages keep only the per-connection forms: no Custom badge,
        // no group delete form, no deletion warning.
        assert!(!catalog_page.contains("<span class=\"badge\">Custom</span>"));
        assert!(!catalog_page.contains("Delete provider"));
        assert!(!catalog_page.contains("/providers/catalog/preset-a/delete"));
        assert!(!catalog_page.contains("Deleting this provider also deletes its connections."));
        assert_eq!(catalog_page.matches("name=\"_csrf\"").count(), 3);

        let form = ProviderFormPage {
            app_name: crate::APP_NAME,
            csrf_token: CSRF.to_string(),
            is_edit: true,
            action: "/providers/p1".to_string(),
            name: "Local <LLM>".to_string(),
            base_url: "http://127.0.0.1:8080/v1".to_string(),
            enabled: false,
            protocols: protocol_options(Some(&Protocol::OpenAi)),
            protocol: Protocol::OpenAi.as_str().to_string(),
            key_placeholder: "Stored securely; leave blank to keep",
            endpoint_locked: false,
            back_href: "/providers".to_string(),
        }
        .render()
        .unwrap();
        assert!(form.contains("action=\"/providers/p1\""));
        assert!(form.contains("placeholder=\"Stored securely; leave blank to keep\""));
        assert!(form.contains("<option value=\"openai\" selected>OpenAI</option>"));
        // Disabled checkbox renders without a checked attribute.
        assert!(!form.contains("checked"));
        assert!(form.contains("name=\"_csrf\""));
        assert!(form.contains("href=\"/providers\""));

        let locked = ProviderFormPage {
            app_name: crate::APP_NAME,
            csrf_token: CSRF.to_string(),
            is_edit: true,
            action: "/providers/p2".to_string(),
            name: "Grouped <LLM>".to_string(),
            base_url: "http://127.0.0.1:8080/v1".to_string(),
            enabled: true,
            protocols: protocol_options(Some(&Protocol::OpenAi)),
            protocol: Protocol::OpenAi.as_str().to_string(),
            key_placeholder: "Stored securely; leave blank to keep",
            endpoint_locked: true,
            back_href: "/providers/groups/custom-work".to_string(),
        }
        .render()
        .unwrap();
        // Grouped connections submit the endpoint via hidden fields and show
        // it as read-only metadata instead of editable controls.
        assert!(
            locked.contains("type=\"hidden\" name=\"base_url\" value=\"http://127.0.0.1:8080/v1\"")
        );
        assert!(locked.contains("type=\"hidden\" name=\"protocol\" value=\"openai\""));
        assert!(locked.contains("<code>http://127.0.0.1:8080/v1</code>"));
        assert!(locked.contains("<code>openai</code>"));
        assert!(!locked.contains("<select"));
        assert!(locked.contains("href=\"/providers/groups/custom-work\""));

        let custom = CustomProviderFormPage {
            app_name: crate::APP_NAME,
            csrf_token: CSRF.to_string(),
        }
        .render()
        .unwrap();
        assert!(custom.contains("<title>SparkRouter :: Add custom provider</title>"));
        assert!(custom.contains("action=\"/providers/custom\""));
        assert!(custom.contains("name=\"_csrf\""));
        assert!(custom.contains("name=\"base_url\""));
        assert!(custom.contains("<option value=\"openai\" selected>OpenAI</option>"));
        assert!(custom.contains("<option value=\"anthropic\">Anthropic</option>"));
        // Connections (and their secrets) are added later on the group page.
        assert!(!custom.contains("name=\"api_key\""));
        assert!(!custom.contains("name=\"enabled\""));

        let ok = TestResultFragment {
            ok: true,
            message: "reachable".to_string(),
        }
        .render()
        .unwrap();
        assert!(ok.contains("OK"));
        let fail = TestResultFragment {
            ok: false,
            message: "provider request failed".to_string(),
        }
        .render()
        .unwrap();
        assert!(fail.contains("provider request failed"));
    }
}
