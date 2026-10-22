//! Property-based and unit tests for the embedded-resource content model.
//!
//! Covers the correctness properties from the `acp-v1-full-support` design:
//! - P1 — Embedded-resource round-trip (Requirement 1.2)
//! - P2 — Backward-compatible deserialization (Requirement 1.3)
//! - P3 — Blob size invariant (Requirement 1.4)

use adk_core::{
    BlobResourceContents, Content, EmbeddedResource, MAX_INLINE_DATA_SIZE, Part,
    TextResourceContents,
};
use proptest::prelude::*;

fn arb_uri() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("file:///project/src/main.rs".to_string()),
        Just("file:///notes.txt".to_string()),
        Just("https://example.com/doc.md".to_string()),
        Just("gs://bucket/object".to_string()),
        "[a-z]{3,10}://[a-z0-9/_.-]{1,20}".prop_map(|s| s),
    ]
}

fn arb_mime_type() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        Just(Some("text/plain".to_string())),
        Just(Some("text/markdown".to_string())),
        Just(Some("text/x-rust".to_string())),
        Just(Some("image/png".to_string())),
        Just(Some("application/pdf".to_string())),
        Just(Some("application/octet-stream".to_string())),
    ]
}

fn arb_text_resource() -> impl Strategy<Value = TextResourceContents> {
    (arb_uri(), arb_mime_type(), ".{0,256}")
        .prop_map(|(uri, mime_type, text)| TextResourceContents { uri, mime_type, text })
}

fn arb_blob_resource() -> impl Strategy<Value = BlobResourceContents> {
    (arb_uri(), arb_mime_type(), prop::collection::vec(any::<u8>(), 0..1024))
        .prop_map(|(uri, mime_type, data)| BlobResourceContents { uri, mime_type, data })
}

fn arb_embedded_resource() -> impl Strategy<Value = EmbeddedResource> {
    prop_oneof![
        arb_text_resource().prop_map(EmbeddedResource::Text),
        arb_blob_resource().prop_map(EmbeddedResource::Blob),
    ]
}

fn arb_embedded_resource_part() -> impl Strategy<Value = Part> {
    arb_embedded_resource().prop_map(|resource| Part::EmbeddedResource { resource })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// **Feature: acp-v1-full-support, Property P1: Embedded-resource round-trip**
    /// *For any* `EmbeddedResource_Part` value, serialize-then-deserialize yields an equal value,
    /// preserving the URI, optional MIME type, and text or binary contents.
    /// **Validates: Requirements 1.2**
    #[test]
    fn prop_embedded_resource_round_trip(part in arb_embedded_resource_part()) {
        let json = serde_json::to_string(&part).unwrap();
        let deserialized: Part = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&part, &deserialized);
    }

    /// **Feature: acp-v1-full-support, Property P1: Embedded-resource round-trip (in Content)**
    /// *For any* `Content` carrying embedded-resource parts alongside text, serialize-then-deserialize
    /// preserves every part.
    /// **Validates: Requirements 1.2**
    #[test]
    fn prop_content_with_embedded_resources_round_trip(
        resources in prop::collection::vec(arb_embedded_resource(), 0..4),
        text in ".{0,64}",
    ) {
        let mut content = Content::new("user").with_text(text);
        for resource in resources {
            content = content.with_embedded_resource(resource);
        }
        let json = serde_json::to_string(&content).unwrap();
        let deserialized: Content = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&content.parts, &deserialized.parts);
    }

    /// **Feature: acp-v1-full-support, Property P3: Blob size invariant**
    /// *For any* byte payload no larger than `MAX_INLINE_DATA_SIZE`, the checked
    /// `BlobResourceContents` constructor succeeds; oversized payloads are rejected.
    /// **Validates: Requirements 1.4**
    #[test]
    fn prop_blob_within_limit_constructs(len in 0usize..2048) {
        let data = vec![0u8; len];
        let result = BlobResourceContents::new("file:///a.bin", None, data);
        prop_assert!(result.is_ok());
    }
}

/// **Feature: acp-v1-full-support, Property P2: Backward-compatible deserialization**
/// A `Content` serialized before the embedded-resource variant existed deserializes
/// successfully and preserves all original parts.
/// **Validates: Requirements 1.3**
#[test]
fn old_content_deserializes_without_embedded_resource_field() {
    // Wire format produced before the EmbeddedResource variant was added:
    // text, inline data, file data, thinking, function call/response.
    let legacy_json = r#"{
        "role": "user",
        "parts": [
            {"text": "hello"},
            {"mime_type": "image/png", "data": [137, 80, 78, 71]},
            {"mime_type": "application/pdf", "file_uri": "gs://b/f.pdf"},
            {"thinking": "reasoning", "signature": "sig"},
            {"name": "get_weather", "args": {"city": "NYC"}}
        ]
    }"#;

    let content: Content = serde_json::from_str(legacy_json).expect("legacy content must parse");
    assert_eq!(content.role, "user");
    assert_eq!(content.parts.len(), 5);
    assert_eq!(content.parts[0].text(), Some("hello"));
    assert!(
        matches!(&content.parts[1], Part::InlineData { mime_type, .. } if mime_type == "image/png")
    );
    assert!(
        matches!(&content.parts[2], Part::FileData { file_uri, .. } if file_uri == "gs://b/f.pdf")
    );
    assert!(content.parts[3].is_thinking());
    assert!(matches!(&content.parts[4], Part::FunctionCall { name, .. } if name == "get_weather"));
}

#[test]
fn text_resource_round_trip_preserves_verbatim_text() {
    let part = Part::EmbeddedResource {
        resource: EmbeddedResource::Text(TextResourceContents::new(
            "file:///main.rs",
            Some("text/x-rust".to_string()),
            "fn main() {}",
        )),
    };
    let json = serde_json::to_string(&part).unwrap();
    // Text is stored verbatim (not base64 encoded).
    assert!(json.contains("fn main() {}"));
    let deserialized: Part = serde_json::from_str(&json).unwrap();
    assert_eq!(part, deserialized);
}

#[test]
fn blob_resource_disambiguates_from_text_on_data_field() {
    let blob = Part::EmbeddedResource {
        resource: EmbeddedResource::Blob(
            BlobResourceContents::new(
                "file:///logo.png",
                Some("image/png".to_string()),
                vec![1, 2, 3, 4],
            )
            .unwrap(),
        ),
    };
    let json = serde_json::to_string(&blob).unwrap();
    let deserialized: Part = serde_json::from_str(&json).unwrap();
    assert_eq!(blob, deserialized);
    // The blob variant must not be mistaken for a text resource.
    match deserialized {
        Part::EmbeddedResource { resource: EmbeddedResource::Blob(b) } => {
            assert_eq!(b.data, vec![1, 2, 3, 4]);
            assert_eq!(b.uri, "file:///logo.png");
        }
        other => panic!("expected blob embedded resource, got {other:?}"),
    }
}

#[test]
fn mime_type_omitted_when_none() {
    let part = Part::EmbeddedResource {
        resource: EmbeddedResource::Text(TextResourceContents::new("file:///a.txt", None, "hi")),
    };
    let json = serde_json::to_string(&part).unwrap();
    assert!(!json.contains("mime_type"));
}

#[test]
fn blob_constructor_rejects_oversized_payload() {
    let oversized = vec![0u8; MAX_INLINE_DATA_SIZE + 1];
    let result = BlobResourceContents::new("file:///big.bin", None, oversized);
    let err = result.expect_err("oversized blob must be rejected");
    assert!(err.message.contains("exceeds maximum allowed size"));
}

#[test]
fn blob_constructor_accepts_payload_at_limit() {
    let at_limit = vec![0u8; MAX_INLINE_DATA_SIZE];
    let result = BlobResourceContents::new("file:///max.bin", None, at_limit);
    assert!(result.is_ok());
}

#[test]
fn embedded_resource_accessors() {
    let text = EmbeddedResource::Text(TextResourceContents::new(
        "file:///a.md",
        Some("text/markdown".to_string()),
        "# hi",
    ));
    assert_eq!(text.uri(), "file:///a.md");
    assert_eq!(text.mime_type(), Some("text/markdown"));

    let blob =
        EmbeddedResource::Blob(BlobResourceContents::new("file:///b.bin", None, vec![9]).unwrap());
    assert_eq!(blob.uri(), "file:///b.bin");
    assert_eq!(blob.mime_type(), None);

    let part = Part::EmbeddedResource { resource: text };
    assert_eq!(part.embedded_resource().map(EmbeddedResource::uri), Some("file:///a.md"));
    // Non-embedded parts return None.
    assert!(Part::Text { text: "x".to_string() }.embedded_resource().is_none());
}
