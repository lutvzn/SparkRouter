//! Build-time projection of models.dev metadata into conservative categories.
use serde_json::Value;

pub type Entry = (String, String, String);

pub fn project(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let root: Value = serde_json::from_slice(bytes).map_err(|_| "invalid JSON")?;
    let providers = root.as_object().ok_or("expected provider object")?;
    let mut entries = Vec::new();
    for (provider, data) in providers {
        let Some(models) = data.get("models").and_then(Value::as_object) else {
            continue;
        };
        for (id, model) in models {
            // IDs are exact; canonical IDs and aliases are deliberately ignored.
            if model.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                continue;
            }
            let Some(inputs) = model.pointer("/modalities/input").and_then(Value::as_array) else {
                continue;
            };
            let Some(outputs) = model
                .pointer("/modalities/output")
                .and_then(Value::as_array)
            else {
                continue;
            };
            let has_input = |s: &str| inputs.iter().any(|v| v.as_str() == Some(s));
            let has_output = |s: &str| outputs.iter().any(|v| v.as_str() == Some(s));
            let category = if has_output("image") && !has_output("audio") && !has_output("video") {
                Some("image")
            } else if has_output("video") && !has_output("audio") && !has_output("image") {
                Some("video")
            } else if outputs.len() == 1 && has_output("text") {
                if has_input("audio") && !has_input("text") {
                    Some("stt")
                } else if has_input("text")
                    && model.get("tool_call").and_then(Value::as_bool) == Some(true)
                {
                    // Text modalities alone also describe embeddings/rerankers.
                    // Tool support is positive evidence of a conversational model.
                    Some("text")
                } else {
                    None
                }
            } else {
                // Audio alone cannot distinguish music, speech or realtime chat.
                None
            };
            if let Some(category) = category {
                entries.push((provider.clone(), id.clone(), category.into()));
            }
        }
    }
    entries.sort();
    if entries.is_empty() {
        return Err("no classifiable models".into());
    }
    Ok(entries)
}

pub fn rust_source(entries: &[Entry]) -> String {
    let mut source = String::from("&[\n");
    for (provider, model, category) in entries {
        source.push_str(&format!("({provider:?}, {model:?}, {category:?}),\n"));
    }
    source.push_str("]\n");
    source
}
