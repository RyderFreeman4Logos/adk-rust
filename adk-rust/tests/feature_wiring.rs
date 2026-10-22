//! Smoke tests asserting the umbrella's feature forwards wire their
//! re-exported surface.
//!
//! The PR-tier `feature-coverage` matrix compiles and tests the umbrella with
//! its code-execution opt-ins (`code-tools`, `code-embedded-js`,
//! `code-embedded-python`, `code-docker`, `codeact-monty`). These tests give
//! that entry a real signal: a feature that stops forwarding — or a re-export
//! module that stops compiling — fails here, in the umbrella, rather than in
//! a downstream consumer.

/// Always present: the `adk-core` root re-export needs no feature.
#[test]
fn core_reexports_are_wired() {
    let content = adk_rust::Content::new("user").with_text("hi");
    assert_eq!(content.role, "user");
}

#[cfg(feature = "code-tools")]
mod code_tools {
    use adk_rust::Tool;

    /// `code-tools` turns on `adk-tool/code`, so the language-preset tools
    /// exist under `adk_rust::tool` and construct with their documented names.
    #[test]
    fn code_execution_tools_are_reachable() {
        assert_eq!(adk_rust::tool::PythonCodeTool::new().name(), "python_code");
        assert_eq!(adk_rust::tool::JavaScriptCodeTool::new().name(), "javascript_code");
        assert_eq!(adk_rust::tool::MontyPythonCodeTool::new().name(), "monty_python_code");
        assert_eq!(adk_rust::tool::FrontendCodeTool::react().name(), "frontend_code");
    }
}

#[cfg(feature = "code-embedded-js")]
mod code_embedded_js {
    use adk_rust::code::{CodeExecutor, EmbeddedJsExecutor, ExecutionLanguage};

    /// `code-embedded-js` lights up the live executor in `adk_rust::code`.
    #[test]
    fn embedded_js_executor_is_reachable() {
        let executor = EmbeddedJsExecutor::new();
        assert!(executor.supports_language(&ExecutionLanguage::JavaScript));
    }
}

#[cfg(feature = "code-embedded-python")]
mod code_embedded_python {
    use adk_rust::code::{CodeExecutor, ExecutionLanguage, MontyExecutorBuilder};

    /// `code-embedded-python` lights up the Monty executors in
    /// `adk_rust::code` (the `MontyPythonCodeTool` is covered by the
    /// `code-tools` test above).
    #[test]
    fn monty_executors_are_reachable() {
        let executor = MontyExecutorBuilder::new().build_one_shot().unwrap();
        assert!(executor.supports_language(&ExecutionLanguage::Python));
        let repl = MontyExecutorBuilder::new().build_repl().unwrap();
        assert!(repl.supports_language(&ExecutionLanguage::Python));
    }
}

#[cfg(feature = "code-docker")]
mod code_docker {
    /// `code-docker` lights up the persistent Docker executor. Constructing
    /// one requires a Docker daemon, so this only asserts the type is
    /// nameable — the compile is the signal.
    #[test]
    fn docker_executor_type_is_reachable() {
        fn nameable<T>() {}
        nameable::<adk_rust::code::DockerExecutor>();
    }
}

#[cfg(feature = "codeact")]
mod codeact {
    /// `codeact` forwards `adk-agent/codeact`, exposing the module through
    /// the `agent` glob re-export.
    #[test]
    fn codeact_module_is_reachable() {
        assert!(!adk_rust::agent::codeact::CODEACT_SYSTEM_PROMPT.is_empty());
    }
}

#[cfg(feature = "codeact-monty")]
mod codeact_monty {
    use adk_rust::agent::codeact::CodeRuntime;
    use adk_rust::codeact_monty::MontyRuntime;

    /// `codeact-monty` re-exports the runtime crate and implies `codeact`,
    /// so the runtime constructs and reports its capabilities through the
    /// `CodeRuntime` seam.
    #[test]
    fn monty_runtime_is_reachable() {
        let runtime = MontyRuntime::new();
        assert!(runtime.capabilities().supports_suspension);
    }
}
