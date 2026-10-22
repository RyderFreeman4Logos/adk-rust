//! Property tests for the Monty executors: arbitrary JSON survives the
//! host→interpreter→host round trip, both through the `input` binding and
//! through host-function arguments.
//!
//! Gated behind `#[cfg(feature = "embedded-python")]` — run with:
//! ```bash
//! cargo nextest run -p adk-code --features embedded-python
//! ```
#![cfg(feature = "embedded-python")]

use adk_code::{
    CodeExecutor, ExecutionLanguage, ExecutionPayload, ExecutionRequest, ExecutionStatus,
    MontyExecutorBuilder, SandboxPolicy,
};
use proptest::prelude::*;
use serde_json::{Value, json};

/// JSON values whose Monty projection is lossless: `i64`-range integers
/// (larger integers degrade to float), finite floats, and string-keyed
/// containers.
fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        // Finite floats only: NaN/Infinity have no JSON form.
        any::<f64>().prop_filter("finite", |f| f.is_finite()).prop_map(Value::from),
        "[a-zA-Z0-9 _-]{0,12}".prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::from),
            prop::collection::btree_map("[a-zA-Z_][a-zA-Z0-9_]{0,8}", inner, 0..4)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

fn request(code: &str, input: Option<Value>) -> ExecutionRequest {
    ExecutionRequest {
        language: ExecutionLanguage::Python,
        payload: ExecutionPayload::Source { code: code.to_string() },
        argv: vec![],
        stdin: None,
        input,
        sandbox: SandboxPolicy::strict_python(),
        identity: None,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// `input` → script `input` variable → returned unchanged as output.
    #[test]
    fn input_binding_round_trips(value in json_value()) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let executor = MontyExecutorBuilder::new().build_one_shot().unwrap();
            let result = executor.execute(request("input", Some(value.clone()))).await.unwrap();
            prop_assert_eq!(result.status, ExecutionStatus::Success);
            prop_assert_eq!(result.output, Some(value));
            Ok(())
        })?;
    }

    /// Arbitrary JSON → host-function argument → echoed back → returned
    /// unchanged as output.
    #[test]
    fn host_function_arguments_round_trip(value in json_value()) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let executor = MontyExecutorBuilder::new()
                .function_fn("echo", "Echo the first argument.", |args, _kwargs| async move {
                    Ok(args.into_iter().next().unwrap_or(Value::Null))
                })
                .build_one_shot()
                .unwrap();
            let result = executor.execute(request("echo(input)", Some(value.clone()))).await.unwrap();
            prop_assert_eq!(result.status, ExecutionStatus::Success);
            prop_assert_eq!(result.output, Some(value));
            Ok(())
        })?;
    }
}

#[test]
fn nested_structures_survive_the_echo_path() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let executor = MontyExecutorBuilder::new()
            .function_fn("echo", "Echo the first argument.", |args, _kwargs| async move {
                Ok(args.into_iter().next().unwrap_or(Value::Null))
            })
            .build_one_shot()
            .unwrap();
        let value = json!({
            "list": [1, 2.5, "three", null, true],
            "nested": { "deep": { "deeper": [{"k": "v"}] } },
        });
        let result = executor.execute(request("echo(input)", Some(value.clone()))).await.unwrap();
        assert_eq!(result.output, Some(value));
    });
}
