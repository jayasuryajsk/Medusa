//! macOS Seatbelt sandboxing for model-initiated terminal commands.
//!
//! Commands are wrapped with `/usr/bin/sandbox-exec` and a deny-default
//! profile: reads stay broad (toolchains, dyld), writes are confined to the
//! workspace and temp directories, and network access is denied unless
//! explicitly enabled. Writable roots are never spliced into the profile
//! string — they travel as `-D WRn=<path>` parameters referenced with
//! `(param "WRn")`, which is immune to quote/paren injection and handles
//! spaces. Non-macOS platforms degrade to a clean no-op.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::permissions::PermissionPolicy;

/// Result of the one-shot `sandbox-exec` self-test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxAvailability {
    Available,
    /// Not macOS; sandboxing silently does not apply.
    UnsupportedPlatform,
    /// macOS, but the deprecated-yet-functional `sandbox-exec` binary failed
    /// its self-test; commands run unsandboxed with a one-time notice.
    Broken(String),
}

/// Cached process-wide availability probe. On macOS this runs
/// `sandbox-exec -p '(version 1)(allow default)' /usr/bin/true` exactly once.
pub fn sandbox_availability() -> &'static SandboxAvailability {
    static AVAILABILITY: OnceLock<SandboxAvailability> = OnceLock::new();
    AVAILABILITY.get_or_init(probe_availability)
}

#[cfg(target_os = "macos")]
fn probe_availability() -> SandboxAvailability {
    match Command::new("/usr/bin/sandbox-exec")
        .arg("-p")
        .arg("(version 1)(allow default)")
        .arg("/usr/bin/true")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(output) if output.status.success() => SandboxAvailability::Available,
        Ok(output) => SandboxAvailability::Broken(format!(
            "sandbox-exec self-test failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(error) => SandboxAvailability::Broken(format!("sandbox-exec unavailable: {error}")),
    }
}

#[cfg(not(target_os = "macos"))]
fn probe_availability() -> SandboxAvailability {
    SandboxAvailability::UnsupportedPlatform
}

/// One-time notice when the sandbox should apply but the macOS `sandbox-exec`
/// self-test failed. Returns `Some` exactly once per process; unsupported
/// platforms never notice (sandboxing simply does not apply there).
pub fn take_unavailability_notice() -> Option<String> {
    static EMITTED: AtomicBool = AtomicBool::new(false);
    unavailability_notice(sandbox_availability(), &EMITTED)
}

fn unavailability_notice(
    availability: &SandboxAvailability,
    emitted: &AtomicBool,
) -> Option<String> {
    match availability {
        SandboxAvailability::Broken(reason) if !emitted.swap(true, Ordering::SeqCst) => Some(
            format!("sandbox unavailable: {reason}; commands run unsandboxed"),
        ),
        _ => None,
    }
}

/// Everything one wrapped invocation needs: canonical writable roots and the
/// network stance. `strict` (explore probes) always denies network and marks
/// escalation as unavailable at the policy layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSpec {
    pub writable_roots: Vec<PathBuf>,
    pub allow_network: bool,
    pub strict: bool,
}

/// Build the deny-default Seatbelt profile plus the `-D` key/value params for
/// the writable roots. Paths never enter the profile text.
pub fn build_profile(spec: &SandboxSpec) -> (String, Vec<(String, String)>) {
    let mut profile = String::from(
        "(version 1)\n\
         (deny default)\n\
         (allow file-read*)\n\
         (allow process-exec)\n\
         (allow process-fork)\n\
         (allow sysctl-read)\n\
         (allow mach-lookup)\n\
         (allow signal (target same-sandbox))\n\
         (allow file-write-data (literal \"/dev/null\"))\n\
         (allow file-ioctl (literal \"/dev/dtracehelper\"))\n",
    );

    let mut params = Vec::new();
    for (index, root) in spec.writable_roots.iter().enumerate() {
        let key = format!("WR{index}");
        profile.push_str(&format!(
            "(allow file-write* (subpath (param \"{key}\")))\n"
        ));
        params.push((key, root.to_string_lossy().into_owned()));
    }

    if spec.allow_network && !spec.strict {
        profile.push_str("(allow network*)\n(allow system-socket)\n");
    }

    (profile, params)
}

/// Wrap `$SHELL -lc <command>` in a `sandbox-exec` invocation carrying the
/// profile and writable-root params. The child environment advertises the
/// sandbox (`MEDUSA_SANDBOX=seatbelt`, plus `MEDUSA_SANDBOX_NETWORK_DISABLED=1`
/// when network is denied) so hooks and scripts can adapt.
pub fn wrap_command(spec: &SandboxSpec, shell: &OsStr, command_text: &str, cwd: &Path) -> Command {
    let (profile, params) = build_profile(spec);
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command.arg("-p").arg(profile);
    for (key, value) in params {
        command.arg("-D").arg(format!("{key}={value}"));
    }
    command.arg(shell).arg("-lc").arg(command_text);
    command.current_dir(cwd);
    command.env("MEDUSA_SANDBOX", "seatbelt");
    let network_allowed = spec.allow_network && !spec.strict;
    if !network_allowed {
        command.env("MEDUSA_SANDBOX_NETWORK_DISABLED", "1");
    }
    command
}

/// Conservative check: does this failed command's stderr look like a Seatbelt
/// denial? Used only to decorate output with an escalation hint — it never
/// auto-escalates.
pub fn looks_sandbox_denied(stderr: &str, code: Option<i32>) -> bool {
    if code == Some(0) {
        return false;
    }
    const PATTERNS: &[&str] = &[
        "Operation not permitted",
        "operation not permitted",
        "Could not resolve host",
        "Read-only file system",
        "sandbox-exec:",
    ];
    PATTERNS.iter().any(|pattern| stderr.contains(pattern))
}

/// Expand a leading `~` and canonicalize. Returns `None` for empty or
/// nonexistent paths (Seatbelt matches canonical vnode paths, so a root that
/// cannot canonicalize is useless — e.g. `/tmp` must become `/private/tmp`).
pub fn canonicalize_writable_root(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let expanded = if raw == "~" {
        PathBuf::from(std::env::var_os("HOME")?)
    } else if let Some(rest) = raw.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME")?).join(rest)
    } else {
        PathBuf::from(raw)
    };
    expanded.canonicalize().ok()
}

/// Resolved sandbox stance for a runtime: whether commands are wrapped, the
/// network stance, and extra user-configured writable roots. Loaded from the
/// permission config's `sandbox` section with a `MEDUSA_SANDBOX=on|off`
/// environment override in either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    enabled: bool,
    allow_network: bool,
    extra_writable_roots: Vec<String>,
}

impl SandboxPolicy {
    /// Explicit constructor for callers (and tests) that already resolved the
    /// stance themselves.
    pub fn new(enabled: bool, allow_network: bool, extra_writable_roots: Vec<String>) -> Self {
        Self {
            enabled,
            allow_network,
            extra_writable_roots,
        }
    }

    pub fn load(permissions: &PermissionPolicy) -> Self {
        Self::resolve(permissions, std::env::var("MEDUSA_SANDBOX").ok().as_deref())
    }

    /// Mode default (Open runs trusted; Guarded/Ask/Readonly sandbox), then
    /// the config's explicit `sandbox.enabled`, then the environment override.
    pub fn resolve(permissions: &PermissionPolicy, env_override: Option<&str>) -> Self {
        let settings = permissions.sandbox_settings();
        let mut enabled = settings
            .enabled
            .unwrap_or_else(|| permissions.effective_mode().sandboxes_by_default());
        match env_override
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("on" | "1" | "true") => enabled = true,
            Some("off" | "0" | "false") => enabled = false,
            _ => {}
        }
        Self {
            enabled,
            allow_network: settings.allow_network,
            extra_writable_roots: settings.writable_roots,
        }
    }

    pub fn should_sandbox(&self) -> bool {
        self.enabled
    }

    /// Writable roots: workspace, canonical temp dir, `/tmp` (canonicalizes to
    /// `/private/tmp` on macOS), plus configured extras. `strict` always
    /// denies network, even when `allow_network` is configured.
    pub fn spec(&self, workspace: &Path, strict: bool) -> SandboxSpec {
        let mut writable_roots: Vec<PathBuf> = Vec::new();
        let builtin = [
            workspace.to_path_buf(),
            std::env::temp_dir(),
            PathBuf::from("/tmp"),
        ];
        let extras = self
            .extra_writable_roots
            .iter()
            .filter_map(|root| canonicalize_writable_root(root));
        for candidate in builtin
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .chain(extras)
        {
            if !writable_roots.contains(&candidate) {
                writable_roots.push(candidate);
            }
        }

        SandboxSpec {
            writable_roots,
            allow_network: self.allow_network && !strict,
            strict,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{PermissionMode, PermissionPolicy};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn spec_with_roots(roots: Vec<PathBuf>, allow_network: bool, strict: bool) -> SandboxSpec {
        SandboxSpec {
            writable_roots: roots,
            allow_network,
            strict,
        }
    }

    #[test]
    fn profile_references_roots_via_params_never_by_splicing() {
        let tricky = PathBuf::from("/tmp/space dir/quote\")(allow default)");
        let spec = spec_with_roots(
            vec![PathBuf::from("/workspace"), tricky.clone()],
            false,
            false,
        );

        let (profile, params) = build_profile(&spec);

        assert!(profile.starts_with("(version 1)\n(deny default)\n"));
        assert!(profile.contains("(allow file-write* (subpath (param \"WR0\")))"));
        assert!(profile.contains("(allow file-write* (subpath (param \"WR1\")))"));
        // Paths must never enter the profile text — injection-shaped or not.
        assert!(!profile.contains("/workspace"));
        assert!(!profile.contains("space dir"));
        assert_eq!(
            params,
            vec![
                ("WR0".to_string(), "/workspace".to_string()),
                ("WR1".to_string(), tricky.to_string_lossy().into_owned()),
            ]
        );
    }

    #[test]
    fn network_allow_block_requires_flag_and_non_strict() {
        let roots = vec![PathBuf::from("/workspace")];

        let (denied, _) = build_profile(&spec_with_roots(roots.clone(), false, false));
        assert!(!denied.contains("network"));

        let (allowed, _) = build_profile(&spec_with_roots(roots.clone(), true, false));
        assert!(allowed.contains("(allow network*)"));
        assert!(allowed.contains("(allow system-socket)"));

        // Strict probes never get network, even if configured.
        let (strict, _) = build_profile(&spec_with_roots(roots, true, true));
        assert!(!strict.contains("network"));
    }

    #[test]
    fn wrap_command_sets_sandbox_env_and_params() {
        let spec = spec_with_roots(vec![PathBuf::from("/tmp/wr root")], false, false);
        let command = wrap_command(&spec, OsStr::new("/bin/zsh"), "echo hi", Path::new("/tmp"));

        assert_eq!(command.get_program(), OsStr::new("/usr/bin/sandbox-exec"));
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"-D".to_string()));
        assert!(args.contains(&"WR0=/tmp/wr root".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("echo hi"));
        assert_eq!(args[args.len() - 3], "/bin/zsh");
        assert_eq!(args[args.len() - 2], "-lc");

        let envs: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(envs.contains(&("MEDUSA_SANDBOX".to_string(), Some("seatbelt".to_string()))));
        assert!(envs.contains(&(
            "MEDUSA_SANDBOX_NETWORK_DISABLED".to_string(),
            Some("1".to_string())
        )));
    }

    #[test]
    fn wrap_command_omits_network_disabled_marker_when_network_allowed() {
        let spec = spec_with_roots(vec![PathBuf::from("/tmp")], true, false);
        let command = wrap_command(&spec, OsStr::new("sh"), "true", Path::new("/tmp"));
        assert!(
            !command
                .get_envs()
                .any(|(key, _)| key == OsStr::new("MEDUSA_SANDBOX_NETWORK_DISABLED"))
        );
    }

    #[test]
    fn sandbox_denial_heuristic_is_conservative() {
        for stderr in [
            "touch: /etc/hosts: Operation not permitted",
            "curl: (6) Could not resolve host: example.com",
            "cp: /System/x: Read-only file system",
            "sandbox-exec: profile error",
        ] {
            assert!(looks_sandbox_denied(stderr, Some(1)), "{stderr}");
        }

        // Ordinary failures must not read as sandbox denials.
        for stderr in [
            "error[E0308]: mismatched types",
            "test result: FAILED. 1 passed; 1 failed",
            "assertion failed: left == right",
            "",
        ] {
            assert!(!looks_sandbox_denied(stderr, Some(1)), "{stderr:?}");
        }

        // Successful commands never carry a denial, whatever stderr says.
        assert!(!looks_sandbox_denied("Operation not permitted", Some(0)));
    }

    #[test]
    fn writable_roots_canonicalize_and_expand_home() {
        let tmp = canonicalize_writable_root("/tmp").expect("/tmp exists");
        assert_eq!(tmp, PathBuf::from("/tmp").canonicalize().unwrap());
        #[cfg(target_os = "macos")]
        assert_eq!(tmp, PathBuf::from("/private/tmp"));

        if let Some(home) = std::env::var_os("HOME") {
            let expanded = canonicalize_writable_root("~").expect("home exists");
            assert_eq!(expanded, PathBuf::from(home).canonicalize().unwrap());
        }

        assert_eq!(canonicalize_writable_root(""), None);
        assert_eq!(
            canonicalize_writable_root("/definitely/not/a/real/path/medusa"),
            None
        );
    }

    #[test]
    fn mode_policy_matrix_controls_the_default() {
        let cases = [
            (PermissionMode::Open, false),
            (PermissionMode::Guarded, true),
            (PermissionMode::Ask, true),
            (PermissionMode::Readonly, true),
        ];
        for (mode, expected) in cases {
            let workspace = temp_workspace();
            PermissionPolicy::write_mode(&workspace, mode).unwrap();
            let permissions = PermissionPolicy::load(&workspace).unwrap();
            let policy = SandboxPolicy::resolve(&permissions, None);
            assert_eq!(
                policy.should_sandbox(),
                expected,
                "{} mode default",
                mode.name()
            );
        }
    }

    #[test]
    fn env_override_beats_mode_default_in_both_directions() {
        let workspace = temp_workspace();
        PermissionPolicy::write_mode(&workspace, PermissionMode::Guarded).unwrap();
        let guarded = PermissionPolicy::load(&workspace).unwrap();
        assert!(!SandboxPolicy::resolve(&guarded, Some("off")).should_sandbox());
        assert!(!SandboxPolicy::resolve(&guarded, Some("0")).should_sandbox());

        let workspace = temp_workspace();
        PermissionPolicy::write_mode(&workspace, PermissionMode::Open).unwrap();
        let open = PermissionPolicy::load(&workspace).unwrap();
        assert!(SandboxPolicy::resolve(&open, Some("on")).should_sandbox());
        assert!(SandboxPolicy::resolve(&open, Some("true")).should_sandbox());
        // Unrecognized values fall through to the mode default.
        assert!(!SandboxPolicy::resolve(&open, Some("seatbelt")).should_sandbox());
    }

    #[test]
    fn config_enabled_flag_overrides_mode_default() {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join(".medusa")).unwrap();
        fs::write(
            workspace.join(".medusa/permissions.json"),
            r#"{"mode":"open","sandbox":{"enabled":true,"allow_network":true,"writable_roots":["/tmp"]}}"#,
        )
        .unwrap();
        let permissions = PermissionPolicy::load(&workspace).unwrap();

        let policy = SandboxPolicy::resolve(&permissions, None);
        assert!(policy.should_sandbox());

        let spec = policy.spec(&workspace.canonicalize().unwrap(), false);
        assert!(spec.allow_network);
        // Strict probes deny network even when the config allows it.
        assert!(
            !policy
                .spec(&workspace.canonicalize().unwrap(), true)
                .allow_network
        );
    }

    #[test]
    fn spec_includes_workspace_and_temp_roots_deduplicated() {
        let workspace = temp_workspace().canonicalize().unwrap();
        let policy = SandboxPolicy::new(true, false, vec!["/tmp".to_string()]);

        let spec = policy.spec(&workspace, false);

        assert!(spec.writable_roots.contains(&workspace));
        let canonical_tmp = PathBuf::from("/tmp").canonicalize().unwrap();
        assert_eq!(
            spec.writable_roots
                .iter()
                .filter(|root| **root == canonical_tmp)
                .count(),
            1,
            "duplicate roots must collapse: {:?}",
            spec.writable_roots
        );
        assert!(
            spec.writable_roots
                .contains(&std::env::temp_dir().canonicalize().unwrap())
        );
    }

    #[test]
    fn unavailability_notice_fires_exactly_once() {
        let emitted = AtomicBool::new(false);
        let broken = SandboxAvailability::Broken("self-test failed".to_string());

        let first = unavailability_notice(&broken, &emitted);
        assert!(
            first
                .as_deref()
                .is_some_and(|notice| notice.contains("self-test failed")),
            "{first:?}"
        );
        assert_eq!(unavailability_notice(&broken, &emitted), None);

        // Unsupported platforms and healthy sandboxes never notice.
        let quiet = AtomicBool::new(false);
        assert_eq!(
            unavailability_notice(&SandboxAvailability::UnsupportedPlatform, &quiet),
            None
        );
        assert_eq!(
            unavailability_notice(&SandboxAvailability::Available, &quiet),
            None
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn live_sandbox_allows_workspace_writes_and_blocks_the_rest() {
        if *sandbox_availability() != SandboxAvailability::Available {
            eprintln!("skipping: sandbox-exec unavailable on this machine");
            return;
        }

        // A workspace with a space exercises the -D param path end to end.
        let workspace = temp_workspace_with_space();
        let policy = SandboxPolicy::new(true, false, Vec::new());
        let spec = policy.spec(&workspace, false);
        let cancel = crate::cancel::CancelToken::default();

        // Writes inside the workspace succeed.
        let inside = crate::proc::run_command(
            wrap_command(
                &spec,
                OsStr::new("/bin/sh"),
                "echo hi && touch inside.txt",
                &workspace,
            ),
            None,
            &cancel,
        )
        .unwrap();
        assert!(inside.success, "stderr: {}", inside.stderr);
        assert_eq!(inside.stdout.trim(), "hi");
        assert!(workspace.join("inside.txt").exists());

        // Writes outside every writable root are denied. HOME is never a
        // default root; if the sandbox failed open, clean up and fail loudly.
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME set"));
        let outside = home.join(format!(
            "medusa-sandbox-escape-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        let denied = crate::proc::run_command(
            wrap_command(
                &spec,
                OsStr::new("/bin/sh"),
                &format!("touch '{}'", outside.display()),
                &workspace,
            ),
            None,
            &cancel,
        )
        .unwrap();
        let escaped = outside.exists();
        let _ = fs::remove_file(&outside);
        assert!(!escaped, "sandboxed command wrote outside its roots");
        assert!(!denied.success);
        assert!(
            looks_sandbox_denied(&denied.stderr, denied.code),
            "stderr should look like a sandbox denial: {}",
            denied.stderr
        );
    }

    fn unique_suffix() -> String {
        static TEMP_COUNTER: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let index = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{nanos}-{index}")
    }

    fn temp_workspace() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "medusa-sandbox-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }

    #[cfg(target_os = "macos")]
    fn temp_workspace_with_space() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "medusa sandbox test {}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }
}
