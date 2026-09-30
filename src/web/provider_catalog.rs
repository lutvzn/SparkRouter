//! Static catalog of well-known provider presets for the dashboard.
//!
//! Pure configuration data with no database or runtime dependencies: provider
//! forms offer these as one-click starting points. Cards show protocol badges;
//! the full endpoint is shown on the provider's detail page.

use crate::providers::Protocol;

/// A built-in provider template shown in the dashboard. Public catalog data
/// only (slug, display name, endpoint, protocol adapter); nothing secret.
#[derive(Debug, Clone, Copy)]
pub(super) struct Preset {
    pub slug: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    pub protocol: Protocol,
}

/// Well-known gateways, in dashboard display order.
pub(super) const PRESETS: &[Preset] = &[
    Preset {
        slug: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "anthropic",
        name: "Anthropic",
        base_url: "https://api.anthropic.com/v1",
        protocol: Protocol::Anthropic,
    },
    Preset {
        slug: "openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "groq",
        name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "together",
        name: "Together",
        base_url: "https://api.together.xyz/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "fireworks",
        name: "Fireworks",
        base_url: "https://api.fireworks.ai/inference/v1",
        protocol: Protocol::OpenAi,
    },
];

/// Looks up a preset by its exact slug.
pub(super) fn find(slug: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.slug == slug)
}

/// Returns the preset whose endpoint and protocol both match exactly, after
/// trimming trailing slashes from `base_url`. The same endpoint advertised
/// with a different protocol is a different tile, not a match.
pub(super) fn matching(base_url: &str, protocol: &Protocol) -> Option<&'static Preset> {
    let base_url = base_url.trim_end_matches('/');
    PRESETS
        .iter()
        .find(|preset| preset.base_url == base_url && preset.protocol.as_str() == protocol.as_str())
}

/// Default label for a legacy connection: the preset name when endpoint and
/// protocol match the catalog, otherwise the URL hostname, falling back to a
/// generic label for values that are not parseable URLs. Named custom providers
/// use their stored names instead; detail pages show the full endpoint.
pub(super) fn display_name(base_url: &str, protocol: &Protocol) -> String {
    if let Some(preset) = matching(base_url, protocol) {
        return preset.name.to_string();
    }
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.to_string()))
        .unwrap_or_else(|| "Custom provider".to_string())
}

#[cfg(test)]
mod tests {
    use super::{PRESETS, Protocol, display_name, find, matching};

    #[test]
    fn find_locates_presets_by_exact_slug() {
        let groq = find("groq").expect("groq preset exists");
        assert_eq!(groq.slug, "groq");
        assert_eq!(groq.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(groq.protocol, Protocol::OpenAi);
        assert!(find("Groq").is_none());
        assert!(find("unknown-provider").is_none());
        assert_eq!(PRESETS.len(), 7);
    }

    #[test]
    fn matching_trims_trailing_slash_and_requires_protocol() {
        let anthropic = matching("https://api.anthropic.com/v1/", &Protocol::Anthropic)
            .expect("anthropic preset matches despite trailing slash");
        assert_eq!(anthropic.slug, "anthropic");
        // Same endpoint advertised with the other protocol is not a match.
        assert!(matching("https://api.anthropic.com/v1", &Protocol::OpenAi).is_none());
        // Unknown endpoints never match, even with a catalogued protocol.
        assert!(matching("https://api.example.com/v1", &Protocol::OpenAi).is_none());
    }

    #[test]
    fn display_name_prefers_preset_then_hostname_then_fallback() {
        assert_eq!(
            display_name("https://api.openai.com/v1", &Protocol::OpenAi),
            "OpenAI"
        );
        assert_eq!(
            display_name("https://llm.internal.example:8080/v1", &Protocol::OpenAi),
            "llm.internal.example"
        );
        assert_eq!(
            display_name("not a valid url", &Protocol::Anthropic),
            "Custom provider"
        );
    }
}
