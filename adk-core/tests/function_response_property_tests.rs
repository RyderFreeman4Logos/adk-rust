//! Property-based tests for multimodal FunctionResponseData.

use adk_core::{FileDataPart, FunctionResponseData, InlineDataPart};
use proptest::prelude::*;

fn arb_mime_type() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("image/png".to_string()),
        Just("image/jpeg".to_string()),
        Just("audio/wav".to_string()),
        Just("application/pdf".to_string()),
        Just("video/mp4".to_string()),
    ]
}

fn arb_inline_data_part() -> impl Strategy<Value = InlineDataPart> {
    (arb_mime_type(), prop::collection::vec(any::<u8>(), 0..1024)).prop_map(|(mime_type, data)| {
        InlineDataPart { mime_type, data, uri: None, annotations: None }
    })
}

fn arb_file_uri() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("gs://bucket/file".to_string()),
        Just("https://example.com/file.pdf".to_string()),
        Just("s3://bucket/key".to_string()),
    ]
}

fn arb_file_data_part() -> impl Strategy<Value = FileDataPart> {
    (arb_mime_type(), arb_file_uri()).prop_map(|(mime_type, file_uri)| FileDataPart {
        mime_type,
        file_uri,
        annotations: None,
    })
}

fn arb_json_value() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        Just(serde_json::json!({})),
        Just(serde_json::json!({"status": "ok"})),
        Just(serde_json::json!({"result": 42})),
        Just(serde_json::json!({"items": ["a", "b"]})),
        Just(serde_json::json!({"nested": {"key": "value"}})),
    ]
}

fn arb_function_response_data() -> impl Strategy<Value = FunctionResponseData> {
    (
        "[a-z_]{3,15}",
        arb_json_value(),
        prop::collection::vec(arb_inline_data_part(), 0..3),
        prop::collection::vec(arb_file_data_part(), 0..3),
    )
        .prop_map(|(name, response, inline_data, file_data)| FunctionResponseData {
            name,
            response,
            inline_data,
            file_data,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// **Feature: multimodal-function-responses, Property 1: FunctionResponseData Serialization Round-Trip**
    /// *For any* valid FunctionResponseData, serializing to JSON and deserializing back produces an equal value.
    /// **Validates: Requirements 1.1, 1.3, 1.4, 7.1**
    #[test]
    fn prop_function_response_data_round_trip(frd in arb_function_response_data()) {
        let json = serde_json::to_string(&frd).unwrap();
        let deserialized: FunctionResponseData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&frd, &deserialized);
    }

    /// **Feature: multimodal-function-responses, Property 2: JSON-Only Backward Compatibility**
    /// *For any* FunctionResponseData with only name and response (no inline/file data),
    /// the serialized JSON contains exactly "name" and "response" keys.
    /// **Validates: Requirements 1.2, 2.2, 5.1, 5.2, 5.3**
    #[test]
    fn prop_json_only_backward_compatibility(
        name in "[a-z_]{3,15}",
        response in arb_json_value(),
    ) {
        let frd = FunctionResponseData::new(&name, response);
        let json = serde_json::to_string(&frd).unwrap();
        let map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&json).unwrap();
        prop_assert!(map.contains_key("name"));
        prop_assert!(map.contains_key("response"));
        prop_assert!(!map.contains_key("inline_data"), "JSON-only should not have inline_data key");
        prop_assert!(!map.contains_key("file_data"), "JSON-only should not have file_data key");
        prop_assert_eq!(map.len(), 2);
    }
}
