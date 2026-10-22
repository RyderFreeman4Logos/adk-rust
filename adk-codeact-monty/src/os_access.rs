//! Host-controlled OS access policy for [`MontyRuntime`](crate::MontyRuntime).
//!
//! Monty surfaces every operating-system effect a script attempts — filesystem
//! reads/writes, `os.getenv`/`os.environ`, and `date.today()`/`datetime.now()` —
//! as a [`RunProgress::OsCall`](adk_code::embedded_python::monty::RunProgress::OsCall)
//! the host must resolve. These calls are **not** tools: they never pause the agent loop and
//! never surface as a [`RunStep::Call`](adk_agent::codeact::RunStep). The
//! runtime services them in place, bounded by the policy described here, and
//! resumes the interpreter immediately.
//!
//! [`OsAccess`] is that policy:
//!
//! - **Filesystem.** Only the directories explicitly mounted with
//!   [`OsAccessBuilder::allow_path`] are reachable, each as read-only or
//!   read-write. A script reaches them through `pathlib.Path` against the
//!   *virtual* mount path. Monty's [`MountTable`] enforces the boundary
//!   (canonicalization + symlink-escape detection), so a script can never touch
//!   a host path outside a mount. Any access outside every mount raises
//!   `PermissionError` (existence checks return `False`, matching CPython).
//! - **Environment.** `os.getenv(name)` and `os.environ` read the explicit
//!   string map supplied with [`OsAccessBuilder::environ`]. The map is empty by
//!   default, so by default the process environment (and any secrets in it) is
//!   never exposed.
//! - **Clock.** `date.today()` and `datetime.now()` read the host clock when
//!   [`OsAccessBuilder::system_clock`] is enabled (the default), and otherwise
//!   raise a catchable `OSError`.
//!
//! Network and subprocess access have no Monty OS-call surface at all, so they
//! remain unavailable regardless of policy.
//!
//! Call servicing itself (environment lookup, clock reads, the mount-table
//! fallback semantics) is shared with the `adk-code` Monty executors through
//! [`adk_code::embedded_python::resolve_os_call`], so the two integrations
//! cannot drift.

use std::collections::BTreeMap;
use std::path::PathBuf;

use adk_code::embedded_python::monty_fs::MountTable;
use adk_code::embedded_python::monty_types::{ExtFunctionResult, OsFunctionCall};
use adk_code::embedded_python::{SUPPORTED_PATH_METHODS, resolve_os_call};

use adk_agent::codeact::RuntimeError;

pub use adk_code::embedded_python::PathAccess;

/// One host directory mounted at a virtual path, with an access mode.
#[derive(Debug, Clone)]
struct MountSpec {
    /// The absolute virtual path a script uses (e.g. `/data`).
    virtual_path: String,
    /// The real host directory backing it.
    host_path: PathBuf,
    /// Whether the script may write through this mount.
    access: PathAccess,
}

/// The OS-access policy a [`MontyRuntime`](crate::MontyRuntime) enforces while
/// driving a script.
///
/// Build one with [`OsAccess::builder`]; the default
/// ([`OsAccess::sandboxed`]) grants no filesystem access and an empty
/// environment, but keeps the host clock available.
#[derive(Debug, Clone)]
pub struct OsAccess {
    mounts: Vec<MountSpec>,
    environ: BTreeMap<String, String>,
    system_clock: bool,
}

impl OsAccess {
    /// A fully sandboxed policy: no filesystem access, an empty environment, and
    /// the host clock still available (`date.today()` / `datetime.now()`).
    #[must_use]
    pub fn sandboxed() -> Self {
        Self { mounts: Vec::new(), environ: BTreeMap::new(), system_clock: true }
    }

    /// Start building a policy.
    #[must_use]
    pub fn builder() -> OsAccessBuilder {
        OsAccessBuilder::new()
    }

    /// Recover a builder seeded with this policy's settings, for further tweaks.
    #[must_use]
    pub fn into_builder(self) -> OsAccessBuilder {
        OsAccessBuilder {
            mounts: self.mounts,
            environ: self.environ,
            system_clock: self.system_clock,
        }
    }

    /// `true` when nothing beyond the (optional) clock is exposed: no mounts and
    /// an empty environment. Used to render the briefing concisely.
    fn is_filesystem_and_env_sandboxed(&self) -> bool {
        self.mounts.is_empty() && self.environ.is_empty()
    }

    /// Assemble a fresh [`MountTable`] for one run.
    ///
    /// A table is built per advance rather than shared, so concurrent runs of
    /// the same runtime never contend on mount state.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Internal`] if a configured host path does not
    /// exist or is not a directory — a host misconfiguration, not a script
    /// error.
    pub(crate) fn build_mount_table(&self) -> Result<MountTable, RuntimeError> {
        let mut table = MountTable::new();
        for spec in &self.mounts {
            table
                .mount(&spec.virtual_path, &spec.host_path, spec.access.mount_mode(), None)
                .map_err(|err| {
                    RuntimeError::Internal(format!(
                        "failed to mount {:?} at {:?}: {}",
                        spec.host_path, spec.virtual_path, err
                    ))
                })?;
        }
        Ok(table)
    }

    /// Resolve a single OS call against this policy, producing the value (or
    /// exception) to resume the interpreter with. Delegates to the shared
    /// servicing kernel in `adk-code`.
    pub(crate) fn resolve(
        &self,
        call: OsFunctionCall,
        mounts: &mut MountTable,
    ) -> ExtFunctionResult {
        resolve_os_call(call, &self.environ, self.system_clock, mounts)
    }

    /// Render the OS-access section appended to the system prompt, describing
    /// exactly what the script may touch.
    pub(crate) fn prompt_section(&self) -> String {
        if self.is_filesystem_and_env_sandboxed() {
            let clock = if self.system_clock {
                " `date.today()` and `datetime.now()` read the host clock."
            } else {
                ""
            };
            return format!(
                "OS access: this is a sandbox. There is no filesystem access (every path is \
                 inaccessible) and `os.environ` is empty. Network and subprocess access are not \
                 available.{clock}"
            );
        }

        let mut section = String::from("OS access (sandboxed):\n");

        if self.mounts.is_empty() {
            section.push_str(
                "- Filesystem: no paths are accessible; any `pathlib.Path` read/write raises \
                 PermissionError.\n",
            );
        } else {
            section.push_str(
                "- Filesystem: use `pathlib.Path` against these mounted paths only; every other \
                 path raises PermissionError (existence checks return False):\n",
            );
            for spec in &self.mounts {
                section.push_str(&format!(
                    "    - {:?} ({})\n",
                    spec.virtual_path,
                    spec.access.label()
                ));
            }
            section.push_str(SUPPORTED_PATH_METHODS);
        }

        if self.environ.is_empty() {
            section.push_str(
                "- Environment: `os.getenv(name)` returns its default and `os.environ` is empty.\n",
            );
        } else {
            section.push_str(&format!(
                "- Environment: `os.getenv(name)` and `os.environ` expose {} variable(s).\n",
                self.environ.len()
            ));
        }

        if self.system_clock {
            section.push_str("- Clock: `date.today()` and `datetime.now()` read the host clock.\n");
        } else {
            section.push_str("- Clock: `date.today()` / `datetime.now()` are unavailable.\n");
        }

        section.push_str("- Network and subprocess access are not available.");
        section
    }
}

impl Default for OsAccess {
    fn default() -> Self {
        Self::sandboxed()
    }
}

/// Builder for [`OsAccess`].
#[derive(Debug, Clone)]
pub struct OsAccessBuilder {
    mounts: Vec<MountSpec>,
    environ: BTreeMap<String, String>,
    system_clock: bool,
}

impl OsAccessBuilder {
    /// A builder seeded with the fully sandboxed defaults: no mounts, an empty
    /// environment, the host clock enabled.
    #[must_use]
    pub fn new() -> Self {
        Self { mounts: Vec::new(), environ: BTreeMap::new(), system_clock: true }
    }

    /// Make a host directory available to scripts at `virtual_path`.
    ///
    /// `virtual_path` is the absolute path a script uses (e.g. `/data`);
    /// `host_path` is the real directory it maps to. The mount boundary is
    /// enforced by Monty — a script can never escape it. Call this repeatedly to
    /// expose several directories.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use adk_codeact_monty::{OsAccess, PathAccess};
    ///
    /// let access = OsAccess::builder()
    ///     .allow_path("/data", "/srv/agent/data", PathAccess::ReadOnly)
    ///     .allow_path("/out", "/srv/agent/out", PathAccess::ReadWrite)
    ///     .build();
    /// # let _ = access;
    /// ```
    #[must_use]
    pub fn allow_path(
        mut self,
        virtual_path: impl Into<String>,
        host_path: impl Into<PathBuf>,
        access: PathAccess,
    ) -> Self {
        self.mounts.push(MountSpec {
            virtual_path: virtual_path.into(),
            host_path: host_path.into(),
            access,
        });
        self
    }

    /// Replace the environment map exposed via `os.getenv` / `os.environ`.
    ///
    /// Only the entries provided here are visible to scripts; the host process
    /// environment is never exposed implicitly.
    #[must_use]
    pub fn environ<K, V>(mut self, vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.environ = vars.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        self
    }

    /// Add or overwrite a single environment variable.
    #[must_use]
    pub fn environ_var(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.environ.insert(key.into(), value.into());
        self
    }

    /// Enable or disable host-clock access (`date.today()` / `datetime.now()`).
    ///
    /// Enabled by default. Disable it for fully deterministic runs.
    #[must_use]
    pub fn system_clock(mut self, enabled: bool) -> Self {
        self.system_clock = enabled;
        self
    }

    /// Finish building the policy.
    #[must_use]
    pub fn build(self) -> OsAccess {
        OsAccess { mounts: self.mounts, environ: self.environ, system_clock: self.system_clock }
    }
}

impl Default for OsAccessBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adk_code::embedded_python::monty_types::{ExcType, GetenvArgs, MontyObject};

    #[test]
    fn getenv_returns_value_or_default() {
        let access = OsAccess::builder().environ_var("HOME", "/home/agent").build();
        let mut mounts = access.build_mount_table().unwrap();

        let hit = access.resolve(
            OsFunctionCall::Getenv(GetenvArgs {
                key: "HOME".to_string(),
                default: MontyObject::None,
            }),
            &mut mounts,
        );
        assert!(
            matches!(hit, ExtFunctionResult::Return(MontyObject::String(s)) if s == "/home/agent")
        );

        let miss = access.resolve(
            OsFunctionCall::Getenv(GetenvArgs {
                key: "MISSING".to_string(),
                default: MontyObject::String("fallback".to_string()),
            }),
            &mut mounts,
        );
        assert!(
            matches!(miss, ExtFunctionResult::Return(MontyObject::String(s)) if s == "fallback")
        );
    }

    #[test]
    fn environ_projects_the_configured_map() {
        let access = OsAccess::builder().environ_var("A", "1").environ_var("B", "2").build();
        let mut mounts = access.build_mount_table().unwrap();

        let ExtFunctionResult::Return(MontyObject::Dict(pairs)) =
            access.resolve(OsFunctionCall::GetEnviron, &mut mounts)
        else {
            panic!("expected a dict from os.environ");
        };
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn unmounted_read_is_a_permission_error_but_existence_is_false() {
        let access = OsAccess::sandboxed();
        let mut mounts = access.build_mount_table().unwrap();

        let read = access.resolve(OsFunctionCall::ReadText("/etc/passwd".into()), &mut mounts);
        match read {
            ExtFunctionResult::Error(exc) => assert_eq!(exc.exc_type(), ExcType::PermissionError),
            other => panic!("expected PermissionError, got {other:?}"),
        }

        let exists = access.resolve(OsFunctionCall::Exists("/etc/passwd".into()), &mut mounts);
        assert!(matches!(exists, ExtFunctionResult::Return(MontyObject::Bool(false))));
    }

    #[test]
    fn disabled_clock_refuses_date_calls() {
        let access = OsAccess::builder().system_clock(false).build();
        let mut mounts = access.build_mount_table().unwrap();

        let today = access.resolve(OsFunctionCall::DateToday, &mut mounts);
        match today {
            // The shared servicing kernel raises a catchable OSError for an
            // ungranted clock.
            ExtFunctionResult::Error(exc) => assert_eq!(exc.exc_type(), ExcType::OSError),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn enabled_clock_returns_a_date() {
        let access = OsAccess::sandboxed();
        let mut mounts = access.build_mount_table().unwrap();
        let today = access.resolve(OsFunctionCall::DateToday, &mut mounts);
        assert!(matches!(today, ExtFunctionResult::Return(MontyObject::Date(_))));
    }

    #[test]
    fn prompt_section_lists_mounts_env_and_supported_path_methods() {
        let section = OsAccess::builder()
            .allow_path("/data", "/srv/data", PathAccess::ReadOnly)
            .environ_var("TOKEN", "x")
            .build()
            .prompt_section();
        assert!(section.contains("\"/data\" (read-only)"), "{section}");
        assert!(section.contains("expose 1 variable"), "{section}");
        // The exact pathlib.Path subset Monty supports must be spelled out so
        // the model does not reach for unsupported methods.
        assert!(section.contains("subset of `pathlib.Path`"), "{section}");
        assert!(section.contains("`read_text()`"), "{section}");
        assert!(section.contains("`iterdir()`"), "{section}");
        assert!(section.contains("read-write mounts only"), "{section}");
    }

    #[test]
    fn prompt_section_omits_path_methods_when_no_paths_are_mounted() {
        // With only an environment configured (no mounts), the pathlib subset is
        // irrelevant and should not be listed.
        let section = OsAccess::builder().environ_var("TOKEN", "x").build().prompt_section();
        assert!(section.contains("no paths are accessible"), "{section}");
        assert!(!section.contains("subset of `pathlib.Path`"), "{section}");
    }

    #[test]
    fn sandboxed_prompt_section_states_no_access() {
        let section = OsAccess::sandboxed().prompt_section();
        assert!(section.contains("no filesystem access"), "{section}");
        assert!(section.contains("host clock"), "{section}");
    }
}
