//! Host functions callable from Python scripts by bare name.
//!
//! A registered [`HostFunction`] becomes a callable Python function visible to
//! scripts (and to the LLM via the executor's
//! [`prompt_snippet`](crate::CodeExecutor::prompt_snippet)). The registry is
//! validated at `build_*()` time — invalid identifiers, duplicates, and
//! collisions with Monty built-ins are construction errors, not runtime
//! surprises.
//!
//! **Trust note:** host functions run as host code. They are the *user's own*
//! trust boundary, not Monty's — the interpreter sandbox does not contain
//! their side effects.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use monty_types::MontyObject;
use serde_json::{Map, Value};
use thiserror::Error;

/// Error raised by a [`HostFunction`] implementation.
///
/// The message becomes a catchable Python exception at the script's call
/// site, so make it actionable for the model (what went wrong, what to try
/// instead).
#[derive(Debug, Error)]
#[error("{message}")]
pub struct HostFunctionError {
    message: String,
}

impl HostFunctionError {
    /// Create an error carrying `message`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use adk_code::HostFunctionError;
    ///
    /// let err = HostFunctionError::new("city not found; pass an ISO country code");
    /// assert!(err.to_string().contains("city not found"));
    /// ```
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

impl From<String> for HostFunctionError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for HostFunctionError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// Errors raised by `MontyExecutorBuilder::build_one_shot()` /
/// `build_repl()` when the configuration is invalid — a bad host-function
/// registry or a bad filesystem mount.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MontyBuildError {
    /// The function name is not a valid Python identifier (or is a keyword).
    #[error(
        "host function name '{name}' is invalid: {reason}. \
         Use a valid Python identifier (letters, digits, underscores; \
         not starting with a digit; not a Python keyword)."
    )]
    InvalidFunctionName {
        /// The rejected name.
        name: String,
        /// Why it was rejected.
        reason: String,
    },

    /// Two registered functions share the same name.
    #[error("host function name '{0}' is registered more than once; names must be unique")]
    DuplicateFunctionName(String),

    /// The function name shadows a Monty built-in (e.g. `len`, `print`),
    /// which the interpreter resolves internally — the host function would
    /// never be reachable.
    #[error(
        "host function name '{0}' collides with a Python built-in; \
         Monty resolves built-ins internally, so the function would never be called. \
         Choose a different name."
    )]
    BuiltinCollision(String),

    /// The function name is reserved by the executor itself.
    #[error(
        "host function name '{0}' is reserved: the executor binds the request's \
         JSON input to the `input` variable. Choose a different name."
    )]
    ReservedName(String),

    /// A mount's virtual path is not a normalized absolute path.
    #[error(
        "mount path '{path}' is invalid: {reason}. \
         Use a normalized absolute path such as \"/data\"."
    )]
    InvalidMountPath {
        /// The rejected virtual path.
        path: String,
        /// Why it was rejected.
        reason: String,
    },

    /// Two mounts share the same virtual path.
    #[error("mount path '{0}' is registered more than once; virtual paths must be unique")]
    DuplicateMountPath(String),
}

/// A Rust function callable from Python scripts executed by a Monty executor.
///
/// Register implementations with
/// [`MontyExecutorBuilder::function`](crate::MontyExecutorBuilder::function),
/// or use
/// [`MontyExecutorBuilder::function_fn`](crate::MontyExecutorBuilder::function_fn)
/// for the closure-based common case.
///
/// # Example
///
/// ```rust
/// use adk_code::{HostFunction, HostFunctionError};
/// use async_trait::async_trait;
/// use serde_json::{Map, Value, json};
///
/// struct GetWeather;
///
/// #[async_trait]
/// impl HostFunction for GetWeather {
///     fn name(&self) -> &str {
///         "get_weather"
///     }
///
///     fn description(&self) -> &str {
///         "Current weather for a city."
///     }
///
///     fn signature(&self) -> String {
///         "get_weather(city: str) -> dict".to_string()
///     }
///
///     async fn call(
///         &self,
///         args: Vec<Value>,
///         _kwargs: Map<String, Value>,
///     ) -> Result<Value, HostFunctionError> {
///         let city = args
///             .first()
///             .and_then(Value::as_str)
///             .ok_or_else(|| HostFunctionError::new("pass the city as the first argument"))?;
///         Ok(json!({ "city": city, "temp_c": 21 }))
///     }
/// }
/// ```
#[async_trait]
pub trait HostFunction: Send + Sync {
    /// Python-visible function name (must be a valid Python identifier).
    fn name(&self) -> &str;

    /// One-line description — becomes the Python function's docstring and is
    /// surfaced to the LLM through the executor's prompt snippet.
    fn description(&self) -> &str;

    /// Optional signature rendering for the LLM prompt, e.g.
    /// `"get_weather(city: str, unit: str = \"C\") -> dict"`.
    /// Defaults to `"{name}(...)"`.
    fn signature(&self) -> String {
        format!("{}(...)", self.name())
    }

    /// Invoke with positional and keyword arguments (JSON-converted).
    ///
    /// # Errors
    ///
    /// An `Err` becomes a catchable Python exception carrying the message.
    async fn call(
        &self,
        args: Vec<Value>,
        kwargs: Map<String, Value>,
    ) -> Result<Value, HostFunctionError>;
}

/// Future type returned by the closure adapter.
type HostFnFuture = Pin<Box<dyn Future<Output = Result<Value, HostFunctionError>> + Send>>;

/// Boxed closure stored by the adapter.
type HostFnClosure = dyn Fn(Vec<Value>, Map<String, Value>) -> HostFnFuture + Send + Sync;

/// Closure adapter created by `MontyExecutorBuilder::function_fn`.
pub(crate) struct ClosureHostFunction {
    name: String,
    description: String,
    func: Arc<HostFnClosure>,
}

impl ClosureHostFunction {
    pub(crate) fn new<F, Fut>(
        name: impl Into<String>,
        description: impl Into<String>,
        func: F,
    ) -> Self
    where
        F: Fn(Vec<Value>, Map<String, Value>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, HostFunctionError>> + Send + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            func: Arc::new(move |args, kwargs| Box::pin(func(args, kwargs))),
        }
    }
}

#[async_trait]
impl HostFunction for ClosureHostFunction {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn call(
        &self,
        args: Vec<Value>,
        kwargs: Map<String, Value>,
    ) -> Result<Value, HostFunctionError> {
        (self.func)(args, kwargs).await
    }
}

/// Python keywords (3.x hard keywords) — a host function named after one
/// could never be called from a script.
const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// The name the executor binds the request's JSON input to.
pub(crate) const INPUT_BINDING: &str = "input";

/// The validated, immutable host-function registry shared by both executor
/// products. Keyed by Python-visible name.
#[derive(Clone, Default)]
pub(crate) struct FunctionRegistry {
    functions: BTreeMap<String, Arc<dyn HostFunction>>,
}

impl fmt::Debug for FunctionRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FunctionRegistry")
            .field("names", &self.functions.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl FunctionRegistry {
    /// Validate and index the registered functions.
    ///
    /// # Errors
    ///
    /// Returns a [`MontyBuildError`] for an invalid identifier, a duplicate
    /// name, a built-in collision, or a reserved name.
    pub(crate) fn build(functions: Vec<Arc<dyn HostFunction>>) -> Result<Self, MontyBuildError> {
        let mut map: BTreeMap<String, Arc<dyn HostFunction>> = BTreeMap::new();
        for function in functions {
            let name = function.name().to_string();
            validate_name(&name)?;
            if map.insert(name.clone(), function).is_some() {
                return Err(MontyBuildError::DuplicateFunctionName(name));
            }
        }
        Ok(Self { functions: map })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.functions.contains_key(name)
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Arc<dyn HostFunction>> {
        self.functions.get(name)
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.functions.keys().map(String::as_str)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Arc<dyn HostFunction>> {
        self.functions.values()
    }

    /// A corrective message for a failed call, raised into the script as a
    /// catchable exception: either an unregistered bare name, or a registered
    /// name invoked as a method (host functions are bare functions only).
    pub(crate) fn call_failure_message(&self, name: &str, method_call: bool) -> String {
        if method_call && self.contains(name) {
            return format!(
                "'{name}' is a registered host function, not a method; \
                 call {name}(...) as a bare function"
            );
        }
        self.unknown_function_message(name)
    }

    /// A corrective message for a call to an unregistered name, raised into
    /// the script as a catchable exception.
    pub(crate) fn unknown_function_message(&self, name: &str) -> String {
        if self.is_empty() {
            format!("'{name}' is not defined and no host functions are registered")
        } else {
            format!(
                "'{name}' is not defined; registered host functions: {}",
                self.names().collect::<Vec<_>>().join(", ")
            )
        }
    }
}

/// Validate a host function name: a valid Python identifier that is not a
/// keyword, a Monty built-in, or the reserved `input` binding.
fn validate_name(name: &str) -> Result<(), MontyBuildError> {
    let invalid = |reason: &str| MontyBuildError::InvalidFunctionName {
        name: name.to_string(),
        reason: reason.to_string(),
    };
    let mut chars = name.chars();
    match chars.next() {
        None => return Err(invalid("the name is empty")),
        Some(first) if !first.is_ascii_alphabetic() && first != '_' => {
            return Err(invalid("the first character must be a letter or underscore"));
        }
        Some(_) => {}
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(invalid("only letters, digits, and underscores are allowed"));
    }
    if PYTHON_KEYWORDS.contains(&name) {
        return Err(invalid("the name is a Python keyword"));
    }
    if name == INPUT_BINDING {
        return Err(MontyBuildError::ReservedName(name.to_string()));
    }
    if MontyObject::builtin_function_from_name(name).is_some() {
        return Err(MontyBuildError::BuiltinCollision(name.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn closure_fn(name: &str) -> Arc<dyn HostFunction> {
        Arc::new(ClosureHostFunction::new(name, "test function", |args, _kwargs| async move {
            Ok(json!(args.len()))
        }))
    }

    #[test]
    fn valid_names_build() {
        let registry =
            FunctionRegistry::build(vec![closure_fn("get_weather"), closure_fn("_private2")])
                .unwrap();
        assert!(registry.contains("get_weather"));
        assert!(registry.contains("_private2"));
    }

    #[test]
    fn invalid_identifier_is_rejected() {
        for bad in ["", "2fast", "has-dash", "has space", "emoji🐍"] {
            let err = FunctionRegistry::build(vec![closure_fn(bad)]).unwrap_err();
            assert!(
                matches!(err, MontyBuildError::InvalidFunctionName { .. }),
                "expected InvalidFunctionName for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn keyword_is_rejected() {
        let err = FunctionRegistry::build(vec![closure_fn("lambda")]).unwrap_err();
        assert!(matches!(err, MontyBuildError::InvalidFunctionName { .. }));
    }

    #[test]
    fn duplicate_name_is_rejected() {
        let err = FunctionRegistry::build(vec![closure_fn("dup"), closure_fn("dup")]).unwrap_err();
        assert_eq!(err, MontyBuildError::DuplicateFunctionName("dup".to_string()));
    }

    #[test]
    fn builtin_collision_is_rejected() {
        let err = FunctionRegistry::build(vec![closure_fn("len")]).unwrap_err();
        assert_eq!(err, MontyBuildError::BuiltinCollision("len".to_string()));
    }

    #[test]
    fn reserved_input_binding_is_rejected() {
        let err = FunctionRegistry::build(vec![closure_fn("input")]).unwrap_err();
        assert_eq!(err, MontyBuildError::ReservedName("input".to_string()));
    }

    #[test]
    fn unknown_function_message_lists_registered_names() {
        let registry =
            FunctionRegistry::build(vec![closure_fn("alpha"), closure_fn("beta")]).unwrap();
        let msg = registry.unknown_function_message("gamma");
        assert!(msg.contains("gamma"));
        assert!(msg.contains("alpha"));
        assert!(msg.contains("beta"));
    }

    #[test]
    fn method_style_call_of_a_registered_name_gets_a_corrective_message() {
        let registry = FunctionRegistry::build(vec![closure_fn("alpha")]).unwrap();
        let msg = registry.call_failure_message("alpha", true);
        assert!(msg.contains("not a method"), "message: {msg}");
        // A bare-name miss keeps the unknown-function wording.
        assert_eq!(
            registry.call_failure_message("gamma", false),
            registry.unknown_function_message("gamma")
        );
    }
}
