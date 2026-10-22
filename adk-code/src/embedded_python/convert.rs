//! Marshalling between the framework's JSON values and Monty's [`MontyObject`].
//!
//! The executor seam speaks `serde_json::Value`: `ExecutionRequest.input` binds
//! a JSON value into the script, host functions receive JSON arguments and
//! return JSON results, and a finished script reports its final expression as
//! JSON (`ExecutionResult.output`). Monty speaks [`MontyObject`]. These two
//! functions bridge the gap.
//!
//! Both directions are **depth-capped**. A script can build arbitrarily deep
//! nesting iteratively (`for _ in range(50_000): x = [x]`) with trivial memory
//! and no `RecursionError`, and an unbounded recursive conversion of that
//! value would overflow the host stack — aborting the whole process for an
//! in-process executor. Beyond [`MAX_CONVERSION_DEPTH`], nesting degrades to
//! the [`DEPTH_PLACEHOLDER`] string.

use monty_types::{DictPairs, MontyObject};
use serde_json::{Map, Number, Value};

/// Maximum nesting depth converted in either direction. Deeper levels degrade
/// to [`DEPTH_PLACEHOLDER`]. Tool I/O legitimately nested this deep does not
/// exist in practice; serde_json's own parser defaults to a 128-level limit.
const MAX_CONVERSION_DEPTH: usize = 64;

/// Substituted for any value nested beyond [`MAX_CONVERSION_DEPTH`].
const DEPTH_PLACEHOLDER: &str = "<truncated: nesting depth limit reached>";

/// Convert a host JSON value into a Monty value, to be injected into a script
/// (an `input` binding, a host function result, a resolved name, ...).
///
/// The mapping is the obvious one. A JSON integer that fits in `i64` becomes an
/// `int`; anything larger (or fractional) becomes a `float`. Objects become
/// `dict`s keyed by their string keys, preserving insertion order. Nesting
/// beyond the conversion depth limit degrades to a placeholder string.
///
/// # Example
///
/// ```rust
/// use adk_code::embedded_python::{json_to_monty, monty_types::MontyObject};
/// use serde_json::json;
///
/// assert_eq!(json_to_monty(json!(42)), MontyObject::Int(42));
/// ```
pub fn json_to_monty(value: Value) -> MontyObject {
    to_monty(value, MAX_CONVERSION_DEPTH)
}

fn to_monty(value: Value, budget: usize) -> MontyObject {
    if budget == 0 {
        return MontyObject::String(DEPTH_PLACEHOLDER.to_string());
    }
    match value {
        Value::Null => MontyObject::None,
        Value::Bool(b) => MontyObject::Bool(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                MontyObject::Int(i)
            } else {
                match n.as_f64() {
                    // u64 values above i64::MAX and all non-integral numbers
                    // fall back to float — good enough for tool I/O, which is
                    // rarely built around > 2^63 integers.
                    Some(f) => MontyObject::Float(f),
                    // Unreachable without serde_json's `arbitrary_precision`
                    // feature; degrade to the decimal string rather than a
                    // silently wrong number.
                    None => MontyObject::String(n.to_string()),
                }
            }
        }
        Value::String(s) => MontyObject::String(s),
        Value::Array(items) => {
            MontyObject::List(items.into_iter().map(|v| to_monty(v, budget - 1)).collect())
        }
        Value::Object(map) => {
            let pairs: Vec<(MontyObject, MontyObject)> = map
                .into_iter()
                .map(|(k, v)| (MontyObject::String(k), to_monty(v, budget - 1)))
                .collect();
            MontyObject::Dict(DictPairs::from(pairs))
        }
    }
}

/// Convert a Monty value produced by a script into host JSON.
///
/// This is the "natural" projection: JSON-native Python values map to their
/// bare JSON form (`42`, `"hi"`, `[...]`, `{"a": 1}`). It is deliberately *not*
/// `serde_json::to_value(obj)` — [`MontyObject`]'s derived `Serialize` is an
/// externally tagged snapshot format (`{"Int": 42}`) meant for binary
/// transport, not human-facing JSON. The rare non-JSON-native value (a
/// `tuple`, `bytes`, a `date`, ...) degrades to its Python `repr` string.
/// Nesting beyond the conversion depth limit degrades to a placeholder string.
///
/// # Example
///
/// ```rust
/// use adk_code::embedded_python::{monty_to_json, monty_types::MontyObject};
/// use serde_json::json;
///
/// let list = MontyObject::List(vec![MontyObject::Int(1), MontyObject::Bool(true)]);
/// assert_eq!(monty_to_json(&list), json!([1, true]));
/// ```
pub fn monty_to_json(obj: &MontyObject) -> Value {
    to_json(obj, MAX_CONVERSION_DEPTH)
}

fn to_json(obj: &MontyObject, budget: usize) -> Value {
    if budget == 0 {
        return Value::String(DEPTH_PLACEHOLDER.to_string());
    }
    match obj {
        MontyObject::None => Value::Null,
        MontyObject::Bool(b) => Value::Bool(*b),
        MontyObject::Int(i) => Value::Number(Number::from(*i)),
        // NaN/Infinity have no JSON form; degrade to null rather than fail.
        MontyObject::Float(f) => Number::from_f64(*f).map_or(Value::Null, Value::Number),
        MontyObject::String(s) => Value::String(s.clone()),
        MontyObject::Path(p) => Value::String(p.clone()),
        // Every Python sequence projects to a JSON array.
        MontyObject::List(items)
        | MontyObject::Tuple(items)
        | MontyObject::Set(items)
        | MontyObject::FrozenSet(items) => {
            Value::Array(items.iter().map(|item| to_json(item, budget - 1)).collect())
        }
        MontyObject::Dict(pairs) => dict_to_json(pairs, budget),
        // Everything else (bytes, datetimes, exceptions, file handles, ...) has
        // no JSON-native shape, so it degrades to its Python `repr` — guarded,
        // because `py_repr` recurses through containers (e.g. a NamedTuple's
        // values) with no depth limit of its own.
        other => Value::String(guarded_repr(other, budget)),
    }
}

/// Project a Monty `dict` to a JSON object, stringifying any non-string key
/// via its Python `repr` (JSON object keys must be strings; script
/// argument/result dicts are string-keyed in practice). Distinct Python keys
/// whose string forms collide (`1` and `"1"`) merge silently — the last one
/// wins, matching `json.dumps` round-trip behavior.
fn dict_to_json(pairs: &DictPairs, budget: usize) -> Value {
    let mut map = Map::new();
    for (key, value) in pairs {
        map.insert(key_string(key, budget), to_json(value, budget - 1));
    }
    Value::Object(map)
}

/// Stringify a Monty value used in a string position (dict key, kwargs key):
/// strings pass through, anything else degrades to its guarded `repr`.
pub(crate) fn monty_key_string(key: &MontyObject) -> String {
    key_string(key, MAX_CONVERSION_DEPTH)
}

fn key_string(key: &MontyObject, budget: usize) -> String {
    match key {
        MontyObject::String(s) => s.clone(),
        other => guarded_repr(other, budget),
    }
}

/// `py_repr`, guarded by a depth probe: `py_repr` itself recurses through
/// containers (a script can key a dict with an iteratively built deep tuple),
/// so it is only called on values shallower than the remaining budget.
fn guarded_repr(obj: &MontyObject, budget: usize) -> String {
    if exceeds_depth(obj, budget) { DEPTH_PLACEHOLDER.to_string() } else { obj.py_repr() }
}

/// Whether `obj` nests deeper than `budget` levels. The recursion is bounded
/// by `budget` itself, so the probe cannot overflow.
fn exceeds_depth(obj: &MontyObject, budget: usize) -> bool {
    if budget == 0 {
        return true;
    }
    match obj {
        MontyObject::List(items)
        | MontyObject::Tuple(items)
        | MontyObject::Set(items)
        | MontyObject::FrozenSet(items)
        | MontyObject::NamedTuple { values: items, .. } => {
            items.iter().any(|item| exceeds_depth(item, budget - 1))
        }
        MontyObject::Dict(pairs) => pairs
            .into_iter()
            .any(|(k, v)| exceeds_depth(k, budget - 1) || exceeds_depth(v, budget - 1)),
        // Non-container variants (and any future ones py_repr renders flat).
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_json_native_values() {
        let cases = [
            json!(null),
            json!(true),
            json!(42),
            json!(3.5),
            json!("hello"),
            json!([1, 2, 3]),
            json!({"nested": {"n": 7}, "list": [1, "two"]}),
        ];
        for case in cases {
            let monty = json_to_monty(case.clone());
            assert_eq!(monty_to_json(&monty), case);
        }
    }

    #[test]
    fn large_u64_degrades_to_float() {
        let big = json!(u64::MAX);
        assert!(matches!(json_to_monty(big), MontyObject::Float(_)));
    }

    #[test]
    fn non_json_native_value_degrades_to_repr() {
        let tuple = MontyObject::Tuple(vec![MontyObject::Int(1), MontyObject::Int(2)]);
        assert_eq!(monty_to_json(&tuple), json!([1, 2]));
        let bytes = MontyObject::Bytes(vec![1, 2]);
        assert!(monty_to_json(&bytes).is_string());
    }

    #[test]
    fn nan_and_infinity_degrade_to_null() {
        for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(monty_to_json(&MontyObject::Float(f)), Value::Null);
        }
    }

    #[test]
    fn path_projects_to_string() {
        let path = MontyObject::Path("/data/report.csv".to_string());
        assert_eq!(monty_to_json(&path), json!("/data/report.csv"));
    }

    #[test]
    fn colliding_dict_keys_last_one_wins() {
        let pairs = DictPairs::from(vec![
            (MontyObject::Int(1), MontyObject::String("int".to_string())),
            (MontyObject::String("1".to_string()), MontyObject::String("str".to_string())),
        ]);
        assert_eq!(monty_to_json(&MontyObject::Dict(pairs)), json!({"1": "str"}));
    }

    #[test]
    fn deep_monty_nesting_degrades_instead_of_overflowing() {
        // Deep enough to prove the 64-level cap, shallow enough that the
        // value's own recursive drop glue stays within the test stack.
        let mut obj = MontyObject::Int(1);
        for _ in 0..2_000 {
            obj = MontyObject::List(vec![obj]);
        }
        let json = monty_to_json(&obj);
        assert!(json.is_array());
        assert!(json.to_string().contains(DEPTH_PLACEHOLDER));
    }

    #[test]
    fn deep_json_nesting_degrades_instead_of_overflowing() {
        // Built directly — `json!([value])` would re-serialize the whole tree
        // on every iteration.
        let mut value = json!(1);
        for _ in 0..2_000 {
            value = Value::Array(vec![value]);
        }
        let monty = json_to_monty(value);
        assert!(matches!(monty, MontyObject::List(_)));
    }

    #[test]
    fn deep_tuple_dict_key_degrades_instead_of_overflowing() {
        let mut key = MontyObject::Tuple(vec![MontyObject::Int(1)]);
        for _ in 0..2_000 {
            key = MontyObject::Tuple(vec![key]);
        }
        let pairs = DictPairs::from(vec![(key, MontyObject::Int(1))]);
        let json = monty_to_json(&MontyObject::Dict(pairs));
        assert_eq!(json, json!({DEPTH_PLACEHOLDER: 1}));
    }

    #[test]
    fn shallow_values_are_unaffected_by_the_depth_cap() {
        let value = json!({"a": [{"b": [{"c": 1}]}]});
        assert_eq!(monty_to_json(&json_to_monty(value.clone())), value);
    }
}
