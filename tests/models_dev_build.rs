#[path = "../build_support/models_dev.rs"]
mod models_dev;

use serde_json::json;

#[test]
fn imports_exact_ids_with_conservative_modality_evidence() {
    let model = |id, inputs, outputs, tools| {
        json!({
            "id": id, "modalities": {"input": inputs, "output": outputs}, "tool_call": tools
        })
    };
    let input = json!({"vendor": {"models": {
        "chat": model("chat", vec!["text", "image"], vec!["text"], true),
        "embedding": model("embedding", vec!["text"], vec!["text"], false),
        "image": model("image", vec!["text"], vec!["text", "image"], false),
        "video": model("video", vec!["text"], vec!["video"], false),
        "stt": model("stt", vec!["audio"], vec!["text"], false),
        "audio": model("audio", vec!["text"], vec!["audio"], false),
        "mixed": model("mixed", vec!["text"], vec!["text", "audio"], true),
        "alias": model("different", vec!["text"], vec!["text"], true),
        "unknown": {"id": "unknown"}
    }}});
    let entries = models_dev::project(&serde_json::to_vec(&input).unwrap()).unwrap();
    assert_eq!(
        entries,
        vec![
            ("vendor".into(), "chat".into(), "text".into()),
            ("vendor".into(), "image".into(), "image".into()),
            ("vendor".into(), "stt".into(), "stt".into()),
            ("vendor".into(), "video".into(), "video".into()),
        ]
    );
    assert!(models_dev::rust_source(&entries).contains("(\"vendor\", \"chat\", \"text\")"));
}

#[test]
fn malformed_or_empty_feeds_are_rejected_for_fallback() {
    for bytes in [
        b"invalid".as_slice(),
        b"[]",
        b"{}",
        b"{\"vendor\":{\"models\":{}}}",
    ] {
        assert!(models_dev::project(bytes).is_err());
    }
}

#[test]
fn bundled_snapshot_is_sorted_unique_and_well_formed() {
    let entries: Vec<models_dev::Entry> =
        serde_json::from_str(include_str!("../data/models-dev-categories.json")).unwrap();
    assert!(!entries.is_empty());
    assert!(
        entries
            .windows(2)
            .all(|pair| (&pair[0].0, &pair[0].1) < (&pair[1].0, &pair[1].1))
    );
    assert!(
        entries.iter().all(|(_, _, category)| matches!(
            category.as_str(),
            "text" | "image" | "video" | "stt"
        ))
    );
}
