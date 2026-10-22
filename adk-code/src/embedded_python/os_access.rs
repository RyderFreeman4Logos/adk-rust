//! Host-granted OS access policy for the Monty executors.
//!
//! Monty surfaces every operating-system effect a script attempts — filesystem
//! reads/writes, `os.getenv`/`os.environ`, and `date.today()`/`datetime.now()` —
//! as an `OsCall` the host must resolve. The drive loop services these calls
//! **in place**, bounded by the policy described here, and resumes the
//! interpreter immediately: OS calls never pause a request and are never
//! surfaced as host functions.
//!
//! [`OsAccess`] is that policy:
//!
//! - **Filesystem.** Only the directories explicitly granted with
//!   [`MontyExecutorBuilder::allow_path`](crate::MontyExecutorBuilder::allow_path)
//!   are reachable, each as read-only or read-write. A script reaches them
//!   through `pathlib.Path` against the *virtual* mount path. Monty's
//!   [`MountTable`] enforces the boundary (canonicalization + symlink-escape
//!   detection), so a script can never touch a host path outside a mount. Any
//!   access outside every mount raises `PermissionError` (existence checks
//!   return `False`, matching CPython).
//! - **Environment.** `os.getenv(name)` and `os.environ` read the explicit
//!   string map granted at construction. The map is empty by default, so the
//!   process environment (and any secrets in it) is never exposed.
//! - **Clock.** `date.today()` and `datetime.now()` read the host clock only
//!   when [`MontyExecutorBuilder::system_clock`](crate::MontyExecutorBuilder::system_clock)
//!   was granted, and otherwise raise.
//!
//! Network and subprocess access have no Monty OS-call surface at all, so they
//! remain unavailable regardless of policy.
//!
//! # Grants vs. per-request policy
//!
//! The builder's grants are the host-authored trust boundary — the *maximum*
//! access any script can have. The per-request
//! [`SandboxPolicy`](crate::SandboxPolicy) may only **narrow** within it: see
//! [`OsAccess::narrowed`]. A request whose policy exceeds the grants is
//! rejected fail-closed before any code runs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use chrono::{Datelike, FixedOffset, Timelike, Utc};
use monty_fs::{MountCallOutcome, MountMode, MountTable};
use monty_types::{
    DictPairs, ExcType, ExtFunctionResult, GetenvArgs, MontyDate, MontyDateTime, MontyException,
    MontyObject, MontyTimeZone, OsFunctionCall,
};

use super::host_fn::MontyBuildError;
use crate::{EnvironmentPolicy, ExecutionError, FilesystemPolicy, SandboxPolicy};

/// Access mode for a path made available to a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccess {
    /// Reads succeed; writes raise `PermissionError`.
    ReadOnly,
    /// Reads and writes both succeed against the real host directory.
    ReadWrite,
}

impl PathAccess {
    /// Human-readable label for prompts (`"read-only"` / `"read-write"`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::ReadWrite => "read-write",
        }
    }

    /// The corresponding [`MountMode`] for Monty's mount table.
    #[must_use]
    pub fn mount_mode(self) -> MountMode {
        match self {
            Self::ReadOnly => MountMode::ReadOnly,
            Self::ReadWrite => MountMode::ReadWrite,
        }
    }
}

/// Whether every `/`-separated segment of an absolute virtual path is a
/// normal component — non-empty, not `.`, not `..`. The check is
/// string-level on purpose: Monty's mount table matches virtual paths as
/// string prefixes, and `Path::components` would silently normalize away the
/// very forms (`.`, `//`, trailing `/`) that break that matching.
fn has_only_normal_segments(path: &str) -> bool {
    path.split('/').skip(1).all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Validate mount virtual paths at `build_*()` time.
///
/// Monty's mount table matches virtual paths as normalized string prefixes,
/// so a non-normalized registration (relative, trailing slash, `.` or `..`
/// components) would pass policy narrowing yet never match at runtime —
/// every access would raise `PermissionError` instead of failing the build.
/// Duplicate virtual paths are ambiguous and also rejected.
pub(crate) fn validate_mounts(mounts: &[MountSpec]) -> Result<(), MontyBuildError> {
    let mut seen = BTreeSet::new();
    for spec in mounts {
        let path = &spec.virtual_path;
        let invalid = |reason: &str| MontyBuildError::InvalidMountPath {
            path: path.clone(),
            reason: reason.to_string(),
        };
        if !path.starts_with('/') {
            return Err(invalid("the path must be absolute"));
        }
        if path == "/" {
            return Err(invalid(
                "mounting the filesystem root is not supported; mount a subdirectory",
            ));
        }
        if !has_only_normal_segments(path) {
            return Err(invalid(
                "'.', '..', empty, and trailing-slash path components are not allowed",
            ));
        }
        if !seen.insert(path.as_str()) {
            return Err(MontyBuildError::DuplicateMountPath(path.clone()));
        }
    }
    Ok(())
}

/// One host directory mounted at a virtual path, with an access mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountSpec {
    /// The absolute virtual path a script uses (e.g. `/data`).
    pub(crate) virtual_path: String,
    /// The real host directory backing it.
    pub(crate) host_path: PathBuf,
    /// Whether the script may write through this mount.
    pub(crate) access: PathAccess,
}

/// The OS-access policy the drive loop enforces while advancing a script.
///
/// The default is fully sandboxed: no mounts, an empty environment, and no
/// clock. `PartialEq` supports the REPL policy-consistency check — a session's
/// effective policy must not vary between calls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OsAccess {
    pub(crate) mounts: Vec<MountSpec>,
    pub(crate) environ: BTreeMap<String, String>,
    pub(crate) system_clock: bool,
}

impl OsAccess {
    /// Narrow these grants by a per-request [`SandboxPolicy`], producing the
    /// effective policy for one call.
    ///
    /// - `FilesystemPolicy::None` / `EnvironmentPolicy::None` grant nothing
    ///   this call, regardless of what the executor could offer.
    /// - Requested paths and variables must be covered by the grants. A grant
    ///   covers its entire directory subtree, so a request for a granted mount
    ///   *or any subdirectory of one* succeeds — the effective mount is the
    ///   requested path, backed by the corresponding host subdirectory. The
    ///   effective access is the weaker of the granted and requested modes
    ///   only when the request itself asks for read-only.
    /// - `NetworkPolicy` is not consulted: Monty has no network surface, so
    ///   every call runs with less network access than any request allows.
    /// - `working_directory` is rejected — the executors have no working
    ///   directory; scripts address files through the granted mount paths.
    /// - A `FilesystemPolicy::Paths` request naming the same root more than
    ///   once (in either list) is contradictory and rejected — Monty's mount
    ///   table would resolve the duplicate by insertion order, not by the
    ///   weaker access.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionError::UnsupportedPolicy`] naming the excess path or
    /// variable when the request exceeds the grants (fail-closed, before any
    /// code runs).
    pub(crate) fn narrowed(&self, policy: &SandboxPolicy) -> Result<Self, ExecutionError> {
        if let Some(dir) = &policy.working_directory {
            return Err(ExecutionError::UnsupportedPolicy(format!(
                "working_directory {dir:?} is not supported by the Monty executors; \
                 scripts address files through the granted mount paths"
            )));
        }
        let mounts = match &policy.filesystem {
            FilesystemPolicy::None => Vec::new(),
            FilesystemPolicy::WorkspaceReadOnly { root } => {
                vec![self.narrowed_mount(root, PathAccess::ReadOnly)?]
            }
            FilesystemPolicy::WorkspaceReadWrite { root } => {
                vec![self.narrowed_mount(root, PathAccess::ReadWrite)?]
            }
            FilesystemPolicy::Paths { read_only, read_write } => {
                let mut seen = BTreeSet::new();
                let mut mounts = Vec::with_capacity(read_only.len() + read_write.len());
                for (roots, access) in
                    [(read_only, PathAccess::ReadOnly), (read_write, PathAccess::ReadWrite)]
                {
                    for root in roots {
                        if !seen.insert(root.as_path()) {
                            return Err(ExecutionError::UnsupportedPolicy(format!(
                                "path {root:?} is requested more than once; list each root \
                                 exactly once, in either read_only or read_write"
                            )));
                        }
                        mounts.push(self.narrowed_mount(root, access)?);
                    }
                }
                mounts
            }
        };

        let environ = match &policy.environment {
            EnvironmentPolicy::None => BTreeMap::new(),
            EnvironmentPolicy::AllowList(names) => {
                let mut environ = BTreeMap::new();
                for name in names {
                    let Some(value) = self.environ.get(name) else {
                        return Err(ExecutionError::UnsupportedPolicy(format!(
                            "environment variable '{name}' is not granted on this executor; \
                             granted variables: {:?}",
                            self.environ.keys().collect::<Vec<_>>()
                        )));
                    };
                    environ.insert(name.clone(), value.clone());
                }
                environ
            }
        };

        Ok(Self { mounts, environ, system_clock: self.system_clock })
    }

    /// Find the granted mount covering `root` at the requested access level.
    ///
    /// A grant covers its whole subtree: requesting a subdirectory of a
    /// granted mount yields an effective mount at the requested virtual path,
    /// backed by the matching host subdirectory. Nested grants resolve to the
    /// most specific one.
    fn narrowed_mount(
        &self,
        root: &PathBuf,
        requested: PathAccess,
    ) -> Result<MountSpec, ExecutionError> {
        // Reject non-normalized requests before prefix matching: a `..`
        // component would survive `starts_with` and remap the host path
        // outside the granted directory, and `.` / empty / trailing-slash
        // components produce a virtual path Monty's string-prefix mount
        // matching never hits.
        let root_str = root.to_str().filter(|s| s.starts_with('/'));
        let Some(root_str) = root_str.filter(|s| has_only_normal_segments(s)) else {
            return Err(ExecutionError::UnsupportedPolicy(format!(
                "path {root:?} must be a normalized absolute path without '.', '..', empty, \
                 or trailing-slash components"
            )));
        };
        let Some(grant) = self
            .mounts
            .iter()
            .filter(|spec| root.starts_with(&spec.virtual_path))
            .max_by_key(|spec| spec.virtual_path.len())
        else {
            return Err(ExecutionError::UnsupportedPolicy(format!(
                "path {root:?} is not covered by any grant on this executor; granted mounts \
                 (each covering its subtree): {:?}",
                self.mounts.iter().map(|m| m.virtual_path.as_str()).collect::<Vec<_>>()
            )));
        };
        if requested == PathAccess::ReadWrite && grant.access == PathAccess::ReadOnly {
            return Err(ExecutionError::UnsupportedPolicy(format!(
                "path {root:?} is granted read-only on this executor; \
                 the request asks for read-write access"
            )));
        }
        let relative = root.strip_prefix(&grant.virtual_path).map_err(|_| {
            ExecutionError::InternalError(format!(
                "grant '{}' matched {root:?} but is not its prefix",
                grant.virtual_path
            ))
        })?;
        if relative.as_os_str().is_empty() {
            return Ok(MountSpec { access: requested, ..grant.clone() });
        }
        // A subdirectory request: back the requested virtual path with the
        // corresponding host subdirectory.
        let host_path = grant.host_path.join(relative);
        if !host_path.is_dir() {
            return Err(ExecutionError::UnsupportedPolicy(format!(
                "path {root:?} is covered by granted mount '{}', but the backing host \
                 directory {host_path:?} does not exist",
                grant.virtual_path
            )));
        }
        Ok(MountSpec { virtual_path: root_str.to_string(), host_path, access: requested })
    }

    /// Assemble a fresh [`MountTable`] for one drive segment.
    ///
    /// A table is built per segment rather than shared, so concurrent runs of
    /// the same executor never contend on mount state.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionError::InternalError`] if a configured host path does
    /// not exist or is not a directory — a host misconfiguration, not a script
    /// error.
    pub(crate) fn build_mount_table(&self) -> Result<MountTable, ExecutionError> {
        let mut table = MountTable::new();
        for spec in &self.mounts {
            table
                .mount(&spec.virtual_path, &spec.host_path, spec.access.mount_mode(), None)
                .map_err(|err| {
                    ExecutionError::InternalError(format!(
                        "failed to mount {:?} at {:?}: {err}",
                        spec.host_path, spec.virtual_path
                    ))
                })?;
        }
        Ok(table)
    }

    /// Resolve a single OS call against this policy, producing the value (or
    /// exception) to resume the interpreter with.
    pub(crate) fn resolve(
        &self,
        call: OsFunctionCall,
        mounts: &mut MountTable,
    ) -> ExtFunctionResult {
        resolve_os_call(call, &self.environ, self.system_clock, mounts)
    }
}

/// Service one Monty OS call against a host-authored policy: an explicit
/// environment map, a clock grant, and a [`MountTable`] for filesystem
/// operations.
///
/// This is the shared servicing kernel used by every Monty integration in the
/// workspace (the executors here and `adk-codeact-monty`'s `MontyRuntime`):
///
/// - `os.getenv(name)` / `os.environ` read `environ` only — the host process
///   environment is never consulted;
/// - `date.today()` / `datetime.now()` read the host clock when `system_clock`
///   is `true`, and otherwise raise a catchable `OSError`;
/// - everything else is a filesystem operation routed through `mounts`. When
///   no mount covers the path, existence checks return `False` (CPython
///   semantics) and any other access raises `PermissionError`.
///
/// The call is consumed: [`MountTable::handle_os_call`] moves a covered
/// write's payload into the backend without a copy, and hands the call back
/// ([`MountCallOutcome::NotHandled`]) when no mount covers it so the fallback
/// can render it.
pub fn resolve_os_call(
    call: OsFunctionCall,
    environ: &BTreeMap<String, String>,
    system_clock: bool,
    mounts: &mut MountTable,
) -> ExtFunctionResult {
    match call {
        OsFunctionCall::Getenv(args) => getenv(environ, args),
        OsFunctionCall::GetEnviron => get_environ(environ),
        OsFunctionCall::DateToday if system_clock => date_today(),
        OsFunctionCall::DateTimeNow(ref tz) if system_clock => datetime_now(tz),
        // Clock not granted: raise a catchable OSError explaining why.
        call @ (OsFunctionCall::DateToday | OsFunctionCall::DateTimeNow(_)) => {
            ExtFunctionResult::Error(MontyException::new(
                ExcType::OSError,
                Some(format!(
                    "{call:?} is not available: the host clock is not granted by the host policy"
                )),
            ))
        }
        // Everything else is a filesystem operation routed through the
        // mount table.
        call => match mounts.handle_os_call(call) {
            MountCallOutcome::Handled(Ok(value)) => ExtFunctionResult::Return(value),
            MountCallOutcome::Handled(Err(err)) => ExtFunctionResult::Error(err.into_exception()),
            // No mount covers this path. Existence checks report `False`
            // (CPython semantics); anything else is a permission error.
            MountCallOutcome::NotHandled(call) if call.is_existence_check() => {
                ExtFunctionResult::Return(MontyObject::Bool(false))
            }
            MountCallOutcome::NotHandled(call) => ExtFunctionResult::Error(call.on_no_handler()),
        },
    }
}

/// Look up an environment variable, falling back to the call's `default`
/// (which Monty already projected to a [`MontyObject`]) when it is unset.
fn getenv(environ: &BTreeMap<String, String>, args: GetenvArgs) -> ExtFunctionResult {
    match environ.get(&args.key) {
        Some(value) => ExtFunctionResult::Return(MontyObject::String(value.clone())),
        None => ExtFunctionResult::Return(args.default),
    }
}

/// Project the whole environment to a `dict[str, str]` for `os.environ`.
fn get_environ(environ: &BTreeMap<String, String>) -> ExtFunctionResult {
    let pairs: Vec<(MontyObject, MontyObject)> = environ
        .iter()
        .map(|(key, value)| (MontyObject::String(key.clone()), MontyObject::String(value.clone())))
        .collect();
    ExtFunctionResult::Return(MontyObject::Dict(DictPairs::from(pairs)))
}

/// Service `date.today()` from the host's local clock.
fn date_today() -> ExtFunctionResult {
    let today = chrono::Local::now().date_naive();
    ExtFunctionResult::Return(MontyObject::Date(MontyDate {
        year: today.year(),
        month: today.month() as u8,
        day: today.day() as u8,
    }))
}

/// Service `datetime.now(tz=...)` from the host clock.
///
/// `tz` is `None` for a naive local datetime, or a fixed-offset
/// [`MontyTimeZone`] for an aware one.
fn datetime_now(tz: &Option<MontyTimeZone>) -> ExtFunctionResult {
    match tz {
        None => {
            let now = chrono::Local::now().naive_local();
            ExtFunctionResult::Return(MontyObject::DateTime(monty_datetime(&now, None, None)))
        }
        Some(zone) => {
            let Some(offset) = FixedOffset::east_opt(zone.offset_seconds) else {
                return ExtFunctionResult::Error(MontyException::new(
                    ExcType::ValueError,
                    Some(format!("invalid timezone offset: {} seconds", zone.offset_seconds)),
                ));
            };
            let now = Utc::now().with_timezone(&offset).naive_local();
            ExtFunctionResult::Return(MontyObject::DateTime(monty_datetime(
                &now,
                Some(zone.offset_seconds),
                zone.name.clone(),
            )))
        }
    }
}

/// Build a [`MontyDateTime`] from a chrono naive datetime and optional offset.
fn monty_datetime(
    naive: &chrono::NaiveDateTime,
    offset_seconds: Option<i32>,
    timezone_name: Option<String>,
) -> MontyDateTime {
    MontyDateTime {
        year: naive.year(),
        month: naive.month() as u8,
        day: naive.day() as u8,
        hour: naive.hour() as u8,
        minute: naive.minute() as u8,
        second: naive.second() as u8,
        // chrono represents leap seconds as nanosecond() >= 10^9; clamp so
        // the microsecond field stays within MontyDateTime's valid range.
        microsecond: (naive.nanosecond() / 1_000).min(999_999),
        offset_seconds,
        timezone_name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::NetworkPolicy;

    fn grants() -> OsAccess {
        OsAccess {
            mounts: vec![
                MountSpec {
                    virtual_path: "/data".to_string(),
                    host_path: PathBuf::from("/tmp/data"),
                    access: PathAccess::ReadOnly,
                },
                MountSpec {
                    virtual_path: "/out".to_string(),
                    host_path: PathBuf::from("/tmp/out"),
                    access: PathAccess::ReadWrite,
                },
            ],
            environ: BTreeMap::from([("PROJECT".to_string(), "acme".to_string())]),
            system_clock: true,
        }
    }

    fn policy(filesystem: FilesystemPolicy, environment: EnvironmentPolicy) -> SandboxPolicy {
        SandboxPolicy {
            network: NetworkPolicy::Disabled,
            filesystem,
            environment,
            timeout: Duration::from_secs(5),
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
            working_directory: None,
        }
    }

    #[test]
    fn none_policies_grant_nothing() {
        let effective =
            grants().narrowed(&policy(FilesystemPolicy::None, EnvironmentPolicy::None)).unwrap();
        assert_eq!(
            effective,
            OsAccess { mounts: Vec::new(), environ: BTreeMap::new(), system_clock: true }
        );
    }

    #[test]
    fn request_within_grants_intersects() {
        let effective = grants()
            .narrowed(&policy(
                FilesystemPolicy::Paths {
                    read_only: vec![PathBuf::from("/out")],
                    read_write: vec![],
                },
                EnvironmentPolicy::AllowList(vec!["PROJECT".to_string()]),
            ))
            .unwrap();
        // A read-write grant narrows to the requested read-only access.
        assert_eq!(effective.mounts.len(), 1);
        assert_eq!(effective.mounts[0].access, PathAccess::ReadOnly);
        assert_eq!(effective.environ.get("PROJECT").map(String::as_str), Some("acme"));
    }

    #[test]
    fn ungranted_path_is_rejected_naming_the_excess() {
        let err = grants()
            .narrowed(&policy(
                FilesystemPolicy::WorkspaceReadOnly { root: PathBuf::from("/secret") },
                EnvironmentPolicy::None,
            ))
            .unwrap_err();
        match err {
            ExecutionError::UnsupportedPolicy(msg) => assert!(msg.contains("/secret")),
            other => panic!("expected UnsupportedPolicy, got {other:?}"),
        }
    }

    #[test]
    fn write_request_on_read_only_grant_is_rejected() {
        let err = grants()
            .narrowed(&policy(
                FilesystemPolicy::WorkspaceReadWrite { root: PathBuf::from("/data") },
                EnvironmentPolicy::None,
            ))
            .unwrap_err();
        match err {
            ExecutionError::UnsupportedPolicy(msg) => {
                assert!(msg.contains("/data"));
                assert!(msg.contains("read-only"));
            }
            other => panic!("expected UnsupportedPolicy, got {other:?}"),
        }
    }

    #[test]
    fn ungranted_environment_variable_is_rejected_naming_the_excess() {
        let err = grants()
            .narrowed(&policy(
                FilesystemPolicy::None,
                EnvironmentPolicy::AllowList(vec!["SECRET".to_string()]),
            ))
            .unwrap_err();
        match err {
            ExecutionError::UnsupportedPolicy(msg) => assert!(msg.contains("SECRET")),
            other => panic!("expected UnsupportedPolicy, got {other:?}"),
        }
    }

    #[test]
    fn getenv_returns_granted_value_or_default() {
        let access = grants();
        let hit = getenv(
            &access.environ,
            GetenvArgs { key: "PROJECT".to_string(), default: MontyObject::None },
        );
        assert!(matches!(hit, ExtFunctionResult::Return(MontyObject::String(s)) if s == "acme"));
        let miss = getenv(
            &access.environ,
            GetenvArgs { key: "MISSING".to_string(), default: MontyObject::None },
        );
        assert!(matches!(miss, ExtFunctionResult::Return(MontyObject::None)));
    }

    #[test]
    fn disabled_clock_raises_os_error() {
        let access = OsAccess::default();
        let mut mounts = access.build_mount_table().unwrap();
        let result = access.resolve(OsFunctionCall::DateToday, &mut mounts);
        match result {
            ExtFunctionResult::Error(exc) => assert_eq!(exc.exc_type(), ExcType::OSError),
            other => panic!("expected an OSError, got {other:?}"),
        }
    }

    #[test]
    fn enabled_clock_returns_a_date() {
        let access = OsAccess { system_clock: true, ..OsAccess::default() };
        let mut mounts = access.build_mount_table().unwrap();
        let result = access.resolve(OsFunctionCall::DateToday, &mut mounts);
        assert!(matches!(result, ExtFunctionResult::Return(MontyObject::Date(_))));
    }

    #[test]
    fn subdirectory_of_a_grant_is_covered_and_remapped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let access = OsAccess {
            mounts: vec![MountSpec {
                virtual_path: "/data".to_string(),
                host_path: dir.path().to_path_buf(),
                access: PathAccess::ReadWrite,
            }],
            ..OsAccess::default()
        };
        let effective = access
            .narrowed(&policy(
                FilesystemPolicy::WorkspaceReadOnly { root: PathBuf::from("/data/sub") },
                EnvironmentPolicy::None,
            ))
            .unwrap();
        assert_eq!(
            effective.mounts,
            vec![MountSpec {
                virtual_path: "/data/sub".to_string(),
                host_path: dir.path().join("sub"),
                access: PathAccess::ReadOnly,
            }]
        );
    }

    #[test]
    fn subdirectory_request_without_backing_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let access = OsAccess {
            mounts: vec![MountSpec {
                virtual_path: "/data".to_string(),
                host_path: dir.path().to_path_buf(),
                access: PathAccess::ReadOnly,
            }],
            ..OsAccess::default()
        };
        let err = access
            .narrowed(&policy(
                FilesystemPolicy::WorkspaceReadOnly { root: PathBuf::from("/data/missing") },
                EnvironmentPolicy::None,
            ))
            .unwrap_err();
        match err {
            ExecutionError::UnsupportedPolicy(msg) => assert!(msg.contains("does not exist")),
            other => panic!("expected UnsupportedPolicy, got {other:?}"),
        }
    }

    #[test]
    fn sibling_with_a_shared_name_prefix_is_not_covered() {
        // `starts_with` is component-wise: /data-x must not match grant /data.
        let err = grants()
            .narrowed(&policy(
                FilesystemPolicy::WorkspaceReadOnly { root: PathBuf::from("/data-x") },
                EnvironmentPolicy::None,
            ))
            .unwrap_err();
        assert!(matches!(err, ExecutionError::UnsupportedPolicy(_)));
    }

    #[test]
    fn requested_paths_with_dot_or_dotdot_components_are_rejected() {
        for bad in ["/data/..", "/data/../out", "/data/./sub", "relative/path"] {
            let err = grants()
                .narrowed(&policy(
                    FilesystemPolicy::WorkspaceReadOnly { root: PathBuf::from(bad) },
                    EnvironmentPolicy::None,
                ))
                .unwrap_err();
            assert!(
                matches!(err, ExecutionError::UnsupportedPolicy(_)),
                "expected UnsupportedPolicy for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn duplicate_roots_across_the_paths_request_are_rejected() {
        // /out is granted read-write, so both narrowings would individually
        // succeed — the duplicate itself must be the rejection.
        let err = grants()
            .narrowed(&policy(
                FilesystemPolicy::Paths {
                    read_only: vec![PathBuf::from("/out")],
                    read_write: vec![PathBuf::from("/out")],
                },
                EnvironmentPolicy::None,
            ))
            .unwrap_err();
        match err {
            ExecutionError::UnsupportedPolicy(msg) => {
                assert!(msg.contains("more than once"), "message: {msg}");
            }
            other => panic!("expected UnsupportedPolicy, got {other:?}"),
        }

        // The same root twice within one list is equally contradictory.
        let err = grants()
            .narrowed(&policy(
                FilesystemPolicy::Paths {
                    read_only: vec![PathBuf::from("/data"), PathBuf::from("/data")],
                    read_write: vec![],
                },
                EnvironmentPolicy::None,
            ))
            .unwrap_err();
        assert!(matches!(err, ExecutionError::UnsupportedPolicy(_)));
    }

    #[test]
    fn working_directory_request_is_rejected() {
        let mut request = policy(FilesystemPolicy::None, EnvironmentPolicy::None);
        request.working_directory = Some(PathBuf::from("/work"));
        let err = grants().narrowed(&request).unwrap_err();
        match err {
            ExecutionError::UnsupportedPolicy(msg) => assert!(msg.contains("working_directory")),
            other => panic!("expected UnsupportedPolicy, got {other:?}"),
        }
    }

    #[test]
    fn get_environ_projects_the_whole_granted_map() {
        let result = get_environ(&grants().environ);
        let ExtFunctionResult::Return(MontyObject::Dict(pairs)) = result else {
            panic!("expected a dict");
        };
        let entries: Vec<_> = (&pairs).into_iter().collect();
        assert_eq!(
            entries,
            vec![&(
                MontyObject::String("PROJECT".to_string()),
                MontyObject::String("acme".to_string())
            )]
        );
    }

    #[test]
    fn datetime_now_honors_a_fixed_offset_and_rejects_an_invalid_one() {
        let valid = datetime_now(&Some(MontyTimeZone {
            offset_seconds: 3600,
            name: Some("CET".to_string()),
        }));
        match valid {
            ExtFunctionResult::Return(MontyObject::DateTime(dt)) => {
                assert_eq!(dt.offset_seconds, Some(3600));
                assert!(dt.microsecond <= 999_999);
            }
            other => panic!("expected a datetime, got {other:?}"),
        }
        let invalid = datetime_now(&Some(MontyTimeZone { offset_seconds: 999_999, name: None }));
        match invalid {
            ExtFunctionResult::Error(exc) => assert_eq!(exc.exc_type(), ExcType::ValueError),
            other => panic!("expected a ValueError, got {other:?}"),
        }
    }

    #[test]
    fn validate_mounts_rejects_non_normalized_virtual_paths() {
        let spec = |virtual_path: &str| MountSpec {
            virtual_path: virtual_path.to_string(),
            host_path: PathBuf::from("/tmp/data"),
            access: PathAccess::ReadOnly,
        };
        for bad in ["data", "/", "/data/", "/data//sub", "/data/../out", "/data/./sub"] {
            let err = validate_mounts(&[spec(bad)]).unwrap_err();
            assert!(
                matches!(err, MontyBuildError::InvalidMountPath { .. }),
                "expected InvalidMountPath for {bad:?}, got {err:?}"
            );
        }
        let err = validate_mounts(&[spec("/data"), spec("/data")]).unwrap_err();
        assert_eq!(err, MontyBuildError::DuplicateMountPath("/data".to_string()));
        validate_mounts(&[spec("/data"), spec("/data/nested")]).unwrap();
    }
}
