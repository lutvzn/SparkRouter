//! Advisory categories for exact upstream IDs. No prefixes, substring
//! matching, aliases, account-access claims, or runtime network calls.
//!
//! Hand-reviewed vendor documentation and media::catalog entries take priority
//! over the build-time models.dev projection. OpenAI Sora is excluded.
use crate::{
    db::{model_categories::Category, providers::Provider},
    providers::{Protocol, media},
};

const MODELS_DEV: &[(&str, &str, &str)] =
    include!(concat!(env!("OUT_DIR"), "/models_dev_categories.rs"));

fn imported_category(provider: &Provider, model: &str) -> Option<Category> {
    lookup_imported(provider, model, MODELS_DEV)
}

fn lookup_imported(
    provider: &Provider,
    model: &str,
    catalog: &[(&str, &str, &str)],
) -> Option<Category> {
    // Exact endpoint AND protocol matching prevents custom hosts inheriting a
    // vendor's classifications merely by reusing a model ID or display name.
    let preset = crate::provider_catalog::matching(&provider.base_url, &provider.protocol)?;
    // OpenAI's discontinued video models are not restored by an external feed.
    if preset.slug == "openai" && matches!(model, "sora-2" | "sora-2-pro") {
        return None;
    }
    let index = catalog
        .binary_search_by(|(vendor, id, _)| (*vendor, *id).cmp(&(preset.slug, model)))
        .ok()?;
    Category::parse(catalog[index].2).ok()
}

pub(crate) fn category(provider: &Provider, model: &str) -> Option<Category> {
    // Hand-reviewed native entries remain authoritative (including audio).
    documented_category(provider, model).or_else(|| imported_category(provider, model))
}

fn documented_category(provider: &Provider, model: &str) -> Option<Category> {
    use Category::*;
    match media::dialect(provider) {
        Protocol::OpenAi => {
            // OpenAI-compatible does not mean OpenAI's model catalog. A custom
            // service can legitimately reuse these strings for other models.
            let url = reqwest::Url::parse(&provider.base_url).ok()?;
            if url.host_str() != Some("api.openai.com") {
                return None;
            }
            match model {
                "gpt-4o" | "gpt-4o-mini" | "gpt-4.1" | "gpt-4.1-mini" | "gpt-4.1-nano" => {
                    Some(Text)
                }
                "gpt-image-1" | "gpt-image-1-mini" | "gpt-image-1.5" | "dall-e-2" | "dall-e-3" => {
                    Some(Image)
                }
                "tts-1" | "tts-1-hd" | "gpt-4o-mini-tts" => Some(Tts),
                "whisper-1"
                | "gpt-4o-transcribe"
                | "gpt-4o-mini-transcribe"
                | "gpt-4o-transcribe-diarize" => Some(Stt),
                _ => None,
            }
        }
        Protocol::MiniMax => match model {
            "image-01" => Some(Image),
            "MiniMax-Hailuo-2.3" => Some(Video),
            "music-2.6" | "music-3.0" => Some(Music),
            "speech-2.8-hd" | "speech-2.8-turbo" => Some(Tts),
            "asr-1.0" => Some(Stt),
            _ => None,
        },
        Protocol::ElevenLabs => match model {
            "eleven_multilingual_v2" | "eleven_flash_v2_5" | "eleven_turbo_v2_5" | "eleven_v3" => {
                Some(Tts)
            }
            "scribe_v1" | "scribe_v2" => Some(Stt),
            "music_v1" | "music_v2" | "music_v2_5" => Some(Music),
            _ => None,
        },
        Protocol::Xai => match model {
            "grok-imagine-image" | "grok-imagine-image-2.0" | "grok-imagine-image-quality" => {
                Some(Image)
            }
            "grok-imagine-video" => Some(Video),
            _ => None,
        },
        Protocol::Deepgram => match model {
            "nova-2" | "nova-3" | "whisper-large" => Some(Stt),
            _ => None,
        },
        Protocol::Fal => match model {
            "fal-ai/flux/dev" => Some(Image),
            "fal-ai/wan/v2.2-a14b/text-to-video" => Some(Video),
            "fal-ai/ace-step" => Some(Music),
            _ => None,
        },
        // Other providers use the imported catalog or metadata/manual classification.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connection_models::ModelField;

    #[test]
    fn imported_lookup_is_exact_and_scoped_to_endpoint_and_protocol() {
        let mut provider = Provider {
            id: "test".into(),
            name: "irrelevant".into(),
            base_url: "https://api.openai.com/v1/".into(),
            protocol: Protocol::OpenAi,
            encrypted_api_key: vec![],
            enabled: true,
        };
        let catalog = [
            ("openai", "new-model", "image"),
            ("openai", "sora-2", "video"),
        ];
        assert_eq!(
            lookup_imported(&provider, "new-model", &catalog),
            Some(Category::Image)
        );
        assert_eq!(
            lookup_imported(&provider, "new-model-latest", &catalog),
            None
        );
        assert_eq!(lookup_imported(&provider, "sora-2", &catalog), None);
        provider.base_url = "https://custom.example/v1".into();
        assert_eq!(lookup_imported(&provider, "new-model", &catalog), None);
        provider.base_url = "https://api.openai.com/other".into();
        assert_eq!(lookup_imported(&provider, "new-model", &catalog), None);
        provider.base_url = "https://api.openai.com/v1".into();
        provider.protocol = Protocol::Anthropic;
        assert_eq!(lookup_imported(&provider, "new-model", &catalog), None);
    }

    #[test]
    fn upstream_metadata_still_overrides_catalog_and_contradictions_block() {
        let provider = Provider {
            id: "test".into(),
            name: "test".into(),
            base_url: "https://api.openai.com/v1".into(),
            protocol: Protocol::OpenAi,
            encrypted_api_key: vec![],
            enabled: true,
        };
        let fields = [ModelField {
            label: "supported_endpoints".into(),
            value: "chat".into(),
        }];
        assert!(Category::Text.matches_model(&provider, "gpt-image-1", &fields));
        assert!(!Category::Image.matches_model(&provider, "gpt-image-1", &fields));
        let fields = [ModelField {
            label: "output modalities".into(),
            value: "audio".into(),
        }];
        assert!(!Category::Image.matches_model(&provider, "gpt-image-1", &fields));
    }
}
