//! Native command planning without filtering harness arguments.

use std::ffi::OsString;
use std::path::PathBuf;

use ai_memory_core::AgentKind;
use anyhow::Result;
use uuid::Uuid;

/// Harnesses with native-session and transcript adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedHarness {
    /// Anthropic Claude Code.
    Claude,
    /// OpenCode.
    OpenCode,
    /// OpenCode 2.0 beta (`opencode2` binary, side-by-side with v1).
    /// Shares v1's config dir, session store, and agent kind; only the
    /// launched executable differs.
    OpenCode2,
}

impl ManagedHarness {
    /// Parse the user-facing command name.
    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "claude" | "claude-code" => Some(Self::Claude),
            "opencode" | "open-code" => Some(Self::OpenCode),
            "opencode2" | "opencode-v2" | "open-code2" => Some(Self::OpenCode2),
            _ => None,
        }
    }

    /// Core agent kind used on the wire and in storage.
    #[must_use]
    pub const fn agent_kind(self) -> AgentKind {
        match self {
            Self::Claude => AgentKind::ClaudeCode,
            Self::OpenCode | Self::OpenCode2 => AgentKind::OpenCode,
        }
    }

    /// Default executable resolved through `PATH`.
    #[must_use]
    pub const fn executable(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
            Self::OpenCode2 => "opencode2",
        }
    }

    /// Stable user-facing name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
            Self::OpenCode2 => "opencode2",
        }
    }
}

/// Whether the planned native invocation participates in session continuity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// Interactive or persisted native session.
    Session,
    /// Native utility/subcommand or explicitly ephemeral invocation. Arguments
    /// are still passed through and repository state is still checkpointed.
    Passthrough,
}

/// Fully constructed native process invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    /// Executable name/path.
    pub program: OsString,
    /// Native argument vector. User arguments retain byte/order identity.
    pub args: Vec<OsString>,
    /// Session id known before launch (generated, linked, or explicit).
    pub expected_session_id: Option<String>,
    /// Native transcript root resolved from explicit arguments or environment.
    pub session_dir: Option<PathBuf>,
    /// Session-bearing versus utility invocation.
    pub mode: LaunchMode,
}

/// Build the transparent resume/create command for one harness.
///
/// User arguments are never validated or rewritten. Adapter-owned session
/// selectors are inserted only when the invocation is session-bearing and the
/// user did not provide an explicit native selector.
pub fn build_launch_plan(
    harness: ManagedHarness,
    executable: Option<OsString>,
    native_args: Vec<OsString>,
    linked_session_id: Option<&str>,
) -> Result<LaunchPlan> {
    let program = executable.unwrap_or_else(|| OsString::from(harness.executable()));
    let mut args = native_args;
    let session_dir = environment_session_dir(harness);
    let mut expected = explicit_session_id(harness, &args);
    let mode = launch_mode(harness, &args);
    if mode == LaunchMode::Session && !has_native_session_selector(harness, &args) {
        match harness {
            ManagedHarness::Claude => {
                let id = linked_session_id
                    .map(str::to_owned)
                    .unwrap_or_else(|| Uuid::new_v4().to_string());
                if linked_session_id.is_some() {
                    args.extend([OsString::from("--resume"), OsString::from(&id)]);
                } else {
                    args.extend([OsString::from("--session-id"), OsString::from(&id)]);
                }
                expected = Some(id);
            }
            ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => {
                if let Some(id) = linked_session_id {
                    if first_arg_is(&args, "run") {
                        args.insert(1, OsString::from(id));
                        args.insert(1, OsString::from("--session"));
                    } else {
                        args.insert(0, OsString::from(id));
                        args.insert(0, OsString::from("--session"));
                    }
                    expected = Some(id.to_string());
                }
            }
        }
    }

    Ok(LaunchPlan {
        program,
        args,
        expected_session_id: expected,
        session_dir,
        mode,
    })
}

/// Apply the wrapper-owned dangerous-mode flag using native harness syntax.
/// Harnesses that already execute tools without a permission gate need no
/// extra argument.
pub fn apply_yolo(harness: ManagedHarness, args: &mut Vec<OsString>) {
    let flag = match harness {
        ManagedHarness::Claude => Some("--dangerously-skip-permissions"),
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => Some("--auto"),
    };
    if let Some(flag) = flag
        && !has_flag(args, &[flag])
    {
        args.push(OsString::from(flag));
    }
}

/// Whether a native invocation may use ai-memory's one-time adoption prompt.
/// Explicit selectors and utility/ephemeral invocations always pass through.
#[must_use]
pub fn allows_native_session_adoption(harness: ManagedHarness, native_args: &[OsString]) -> bool {
    launch_mode(harness, native_args) == LaunchMode::Session
        && !has_native_session_selector(harness, native_args)
        && !noninteractive_invocation(harness, native_args)
}

fn noninteractive_invocation(harness: ManagedHarness, args: &[OsString]) -> bool {
    match harness {
        ManagedHarness::Claude => has_flag(args, &["--print", "-p"]),
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => first_arg_is(args, "run"),
    }
}

fn launch_mode(harness: ManagedHarness, args: &[OsString]) -> LaunchMode {
    if has_flag(args, &["--help", "-h", "--version", "-v"])
        || has_flag(args, &["--no-session", "--no-session-persistence"])
    {
        return LaunchMode::Passthrough;
    }
    let utility = match harness {
        ManagedHarness::Claude => [
            "agents",
            "auth",
            "auto-mode",
            "doctor",
            "install",
            "mcp",
            "plugin",
            "plugins",
            "project",
            "setup-token",
            "ultrareview",
            "update",
            "upgrade",
        ]
        .as_slice(),
        ManagedHarness::OpenCode => [
            "completion",
            "acp",
            "mcp",
            "attach",
            "debug",
            "providers",
            "agent",
            "upgrade",
            "uninstall",
            "serve",
            "web",
            "models",
            "stats",
            "export",
            "import",
            "github",
            "pr",
            "session",
            "plugin",
            "db",
        ]
        .as_slice(),
        // Beta subcommands, verified on `opencode2 v0.0.0-beta-18999`
        // (`opencode2 --help`). `run` stays session-bearing (see
        // `noninteractive_invocation`); `mini` is the minimal interactive
        // UI and also stays session-bearing.
        ManagedHarness::OpenCode2 => [
            "upgrade", "acp", "api", "debug", "console", "auth", "mcp", "plugin", "models",
            "stats", "export", "import", "service", "pair", "serve",
        ]
        .as_slice(),
    };
    let first = args.first().and_then(|arg| arg.to_str());
    if first.is_some_and(|value| utility.contains(&value)) {
        LaunchMode::Passthrough
    } else {
        LaunchMode::Session
    }
}

/// Whether the caller supplied a native resume, continue, fork, or session
/// selector. Wrapper recovery must not override an explicit native choice.
#[must_use]
pub fn has_native_session_selector(harness: ManagedHarness, args: &[OsString]) -> bool {
    match harness {
        ManagedHarness::Claude => has_flag(
            args,
            &["--resume", "-r", "--continue", "-c", "--session-id"],
        ),
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => {
            has_flag(args, &["--session", "-s", "--continue", "-c", "--fork"])
        }
    }
}

fn explicit_session_id(harness: ManagedHarness, args: &[OsString]) -> Option<String> {
    match harness {
        ManagedHarness::Claude => flag_value(args, &["--resume", "-r", "--session-id"]),
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => {
            flag_value(args, &["--session", "-s"])
        }
    }
}

fn first_arg_is(args: &[OsString], expected: &str) -> bool {
    args.first().and_then(|value| value.to_str()) == Some(expected)
}

fn has_flag(args: &[OsString], names: &[&str]) -> bool {
    args.iter().any(|arg| {
        let Some(value) = arg.to_str() else {
            return false;
        };
        names
            .iter()
            .any(|name| value == *name || value.starts_with(&format!("{name}=")))
    })
}

fn flag_value(args: &[OsString], names: &[&str]) -> Option<String> {
    for (index, arg) in args.iter().enumerate() {
        let value = arg.to_str()?;
        for name in names {
            if value == *name {
                return args
                    .get(index + 1)
                    .and_then(|next| next.to_str())
                    .filter(|next| !next.starts_with('-'))
                    .map(str::to_owned);
            }
            if let Some(found) = value.strip_prefix(&format!("{name}="))
                && !found.is_empty()
            {
                return Some(found.to_string());
            }
        }
    }
    None
}

fn environment_session_dir(harness: ManagedHarness) -> Option<PathBuf> {
    environment_session_dir_with(harness, |name| std::env::var_os(name))
}

fn environment_session_dir_with(
    harness: ManagedHarness,
    get: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let value = |name| {
        get(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    match harness {
        ManagedHarness::Claude => value("CLAUDE_CONFIG_DIR").map(|dir| dir.join("projects")),
        // The beta channel keeps v1's `opencode.db` filename (other channels
        // get `opencode-<channel>.db`), so both harnesses share one store.
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => {
            value("XDG_DATA_HOME").map(|dir| dir.join("opencode"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn claude_generates_then_resumes_native_session() {
        let fresh = build_launch_plan(ManagedHarness::Claude, None, vec![], None).unwrap();
        let id = fresh.expected_session_id.clone().unwrap();
        assert_eq!(strings(&fresh.args), ["--session-id", id.as_str()]);

        let resumed = build_launch_plan(
            ManagedHarness::Claude,
            None,
            vec![OsString::from("--model"), OsString::from("opus")],
            Some(&id),
        )
        .unwrap();
        assert_eq!(
            strings(&resumed.args),
            ["--model", "opus", "--resume", id.as_str()]
        );
    }

    #[test]
    fn explicit_native_selector_wins() {
        let plan = build_launch_plan(
            ManagedHarness::OpenCode,
            None,
            vec![OsString::from("--session=chosen"), OsString::from("--auto")],
            Some("linked"),
        )
        .unwrap();
        assert_eq!(strings(&plan.args), ["--session=chosen", "--auto"]);
        assert_eq!(plan.expected_session_id.as_deref(), Some("chosen"));
    }

    #[test]
    fn adoption_is_only_allowed_for_session_launches_without_a_selector() {
        assert!(allows_native_session_adoption(
            ManagedHarness::OpenCode,
            &[OsString::from("--auto")]
        ));
        assert!(!allows_native_session_adoption(
            ManagedHarness::Claude,
            &[OsString::from("--continue")]
        ));
        assert!(!allows_native_session_adoption(
            ManagedHarness::OpenCode,
            &[OsString::from("models")]
        ));
        assert!(!allows_native_session_adoption(
            ManagedHarness::Claude,
            &[OsString::from("--print"), OsString::from("continue here")]
        ));
        assert!(!allows_native_session_adoption(
            ManagedHarness::OpenCode,
            &[OsString::from("run"), OsString::from("continue here")]
        ));
    }

    #[test]
    fn wrapper_yolo_uses_each_harness_native_flag_without_duplicates() {
        for (harness, expected) in [
            (
                ManagedHarness::Claude,
                Some("--dangerously-skip-permissions"),
            ),
            (ManagedHarness::OpenCode, Some("--auto")),
            (ManagedHarness::OpenCode2, Some("--auto")),
        ] {
            let mut args = Vec::new();
            apply_yolo(harness, &mut args);
            apply_yolo(harness, &mut args);
            assert_eq!(
                strings(&args),
                expected.into_iter().collect::<Vec<_>>(),
                "{} yolo mapping",
                harness.as_str()
            );
        }
    }

    #[test]
    fn native_store_environment_overrides_match_harness_layouts() {
        let get = |name: &str| match name {
            "CLAUDE_CONFIG_DIR" => Some(OsString::from("/stores/claude")),
            "XDG_DATA_HOME" => Some(OsString::from("/stores/xdg")),
            _ => None,
        };
        assert_eq!(
            environment_session_dir_with(ManagedHarness::Claude, get).as_deref(),
            Some(std::path::Path::new("/stores/claude/projects"))
        );
        assert_eq!(
            environment_session_dir_with(ManagedHarness::OpenCode, get).as_deref(),
            Some(std::path::Path::new("/stores/xdg/opencode"))
        );
        assert_eq!(
            environment_session_dir_with(ManagedHarness::OpenCode2, get).as_deref(),
            Some(std::path::Path::new("/stores/xdg/opencode"))
        );
    }

    #[test]
    fn utility_subcommands_are_passed_through_without_resume_flags() {
        let plan = build_launch_plan(
            ManagedHarness::Claude,
            None,
            vec![OsString::from("doctor")],
            Some("linked"),
        )
        .unwrap();
        assert_eq!(plan.mode, LaunchMode::Passthrough);
        assert_eq!(strings(&plan.args), ["doctor"]);
    }

    #[test]
    fn utility_names_parse_to_the_kept_adapters() {
        assert_eq!(
            ManagedHarness::from_name("claude"),
            Some(ManagedHarness::Claude)
        );
        assert_eq!(
            ManagedHarness::from_name("claude-code"),
            Some(ManagedHarness::Claude)
        );
        assert_eq!(
            ManagedHarness::from_name("opencode"),
            Some(ManagedHarness::OpenCode)
        );
        assert_eq!(
            ManagedHarness::from_name("opencode2"),
            Some(ManagedHarness::OpenCode2)
        );
        assert_eq!(ManagedHarness::from_name("codex"), None);
        assert_eq!(ManagedHarness::from_name("kiro"), None);
    }
}
