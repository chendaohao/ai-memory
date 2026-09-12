//! `ai-memory install-hooks` — install lifecycle-hook configuration for
//! the chosen agent CLI.
//!
//! Two modes:
//!
//! - **Default (print):** renders the JSON/TOML/TypeScript snippet the
//!   user should merge into their agent CLI's settings file, plus the
//!   absolute paths to the vendored shell scripts. Nothing is written to
//!   disk.
//!
//! - **`--apply` (recommended):** performs an atomic in-place merge into
//!   the target config file. A timestamped backup (`.bak-<unix-ts>`) is
//!   written next to the file before any mutation. Re-runs are
//!   idempotent — a second `--apply` with unchanged content is a no-op
//!   and produces no backup.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::{AgentChoice, CaptureModeArg, InstallHooksArgs, McpClient, ProjectStrategyArg};
use crate::commands::apply_shared::{ApplyOutcome, apply_atomic, mutate_json};
use crate::commands::install_mcp;
use crate::commands::path_util::home_dir;
use crate::commands::render_shared::{
    build_claude_code_payload_with_data_dir, hook_script_for_claude_code,
    local_hook_policy_v1_supported, ts_capture_policy_v1, ts_string_literal,
};
use crate::commands::uninstall::hook_command_is_ours;
use crate::config::{Config, DEFAULT_SERVER_URL};

const CLAUDE_PROMPT_EVENT: &str = "UserPromptSubmit";

/// Claude Code's settings file — hooks live under `hooks`.
/// `$CLAUDE_CONFIG_DIR/settings.json` when the var is set, else
/// `~/.claude/settings.json`.
pub(crate) fn claude_settings_path() -> anyhow::Result<std::path::PathBuf> {
    claude_settings_path_in(std::env::var_os("CLAUDE_CONFIG_DIR"))
}

/// The env value comes in as a parameter so tests can exercise both
/// branches without mutating process env (mirrors
/// `install_mcp::kimi_code_home`).
fn claude_settings_path_in(
    env_override: Option<std::ffi::OsString>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(dir) = crate::commands::path_util::claude_config_dir(env_override) {
        return Ok(dir.join("settings.json"));
    }
    Ok(home_dir()
        .context("could not locate $HOME for ~/.claude/settings.json")?
        .join(".claude")
        .join("settings.json"))
}

/// `zcode.json`) are trust-gated and off by default, so they are never
/// targeted (#512).
pub(crate) fn zcode_config_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.zcode/cli/config.json")?
        .join(".zcode")
        .join("cli")
        .join("config.json"))
}

/// `~/.config/opencode/plugins/ai-memory.ts` — OpenCode's plugin file.
pub(crate) fn opencode_plugin_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.config/opencode")?
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("ai-memory.ts"))
}

/// `$PI_CODING_AGENT_DIR/extensions/ai-memory.ts` when the var is set, else
/// `~/.omp/agent/extensions/ai-memory.ts` — OMP lifecycle extension.
///
/// Run the `install-hooks` subcommand.
///
/// # Errors
/// Returns an error if the hook script directory cannot be located.
pub fn run(config: &Config, mut args: InstallHooksArgs) -> Result<()> {
    let inferred = if args.server_url.is_none() {
        infer_installed_mcp_config(args.agent)?
    } else {
        None
    };
    let server_url = effective_hook_server_url(config, &args, inferred.as_ref());
    let auth_token_owned = args
        .auth_token
        .clone()
        .or_else(|| config.auth.bearer_token.clone())
        .or_else(|| inferred.as_ref().and_then(|mcp| mcp.auth_token.clone()));
    // #552: persist the bearer under the data dir (0600) and keep it OUT of
    // the rendered config. Before this it went onto every hook's command line —
    // as `--auth-token <token>` for native hooks, as an `AI_MEMORY_AUTH_TOKEN=`
    // shell prefix for the script hooks — so it was readable through
    // `/proc/<pid>/cmdline` by any local user for as long as each hook ran, on
    // every tool call.
    //
    // Only `--apply` persists: a printed snippet is the operator's to place, and
    // writing a credential as a side effect of a dry run would be a surprise.
    let persisted = if args.apply
        && let Some(token) = auth_token_owned.as_deref()
    {
        crate::config::store_hook_auth_token(&config.data_dir, token).with_context(|| {
            format!(
                "storing the hook auth token under {}",
                config.data_dir.display()
            )
        })?;
        true
    } else {
        false
    };
    // Renderers take `Option<&str>` and omit the credential entirely when it is
    // `None`, so one decision here covers every agent instead of twenty edits.
    let auth = if persisted {
        None
    } else {
        auth_token_owned.as_deref()
    };
    // P1.8 multi-user attribution: `--as-user` is metadata only — the
    // token stamped into the hook env block is whatever the operator
    // passed via `--auth-token` (typically a native key from
    // `ai-memory api-key add`). We surface the username to stderr so the
    // operator can confirm which identity their writes will attribute
    // to. Mismatch between `--as-user` and the actual token's owner is
    // the operator's concern; we don't reach back to the server to
    // verify (keeps install-hooks offline-capable).
    validate_as_user(args.as_user.as_deref(), auth)?;
    if let Some(user) = args.as_user.as_deref().filter(|s| !s.trim().is_empty()) {
        eprintln!("[ai-memory] hooks installing for user: {user}");
    }
    let generated = matches!(args.agent, AgentChoice::OpenCode | AgentChoice::OpenCode2);
    if generated || local_hook_policy_v1_supported() {
        eprintln!(
            "[ai-memory] capture-policy capability v1 enforced by this selected integration; re-run --apply to refresh existing installs."
        );
    } else {
        eprintln!(
            "[ai-memory] selected shell/PowerShell compatibility path does not enforce capture-policy v1; use a native platform selection or generated integration."
        );
    }
    // Assistant/Stop capture is Claude Code + native-platform only (#196). No
    // silent fallback: bail so an operator on a script-fallback platform or a
    // different agent is told the flag has no effect instead of installing a
    // command whose capture would be silently dropped.
    if args.capture_assistant && !capture_assistant_allowed(args.agent) {
        anyhow::bail!(
            "--capture-assistant requires --agent claude-code on a native hook platform \
             (PosixNative/WindowsNative). The current selection uses the script fallback or a \
             different agent, where the opt-in cannot take effect. Remove --capture-assistant or \
             switch to a native Claude Code install."
        );
    }
    if (args.no_capture_prompts || args.capture_prompts)
        && !prompt_capture_options_allowed(args.agent)
    {
        anyhow::bail!(
            "--no-capture-prompts and --capture-prompts require --agent claude-code. Other \
             agents may use their prompt hook to deliver handoff context, so removing it could \
             break cross-agent continuity."
        );
    }
    if args.apply {
        // #446: settle the capture failure mode before any agent-specific
        // work, and say which mode is in force. A protection the operator
        // cannot see is one they cannot trust.
        let capture_mode = persist_capture_mode(&config.data_dir, args.capture_mode)?;
        println!("capture mode: {capture_mode}");
        if capture_mode == "allowlist" {
            println!("  repositories without a .ai-memory.toml marker emit no lifecycle events");
            // The gate runs inside the native hook binary (immediately before
            // the spool write) and, since #661, inside the generated
            // TypeScript integrations' shared `capturePolicy` (before any
            // POST). The only install left that reaches the server with no
            // gate at all is the raw script fallback: it POSTs directly and
            // never runs either enforcement point. Saying nothing would leave
            // an operator trusting a protection this install does not have —
            // the exact failure #446 is about.
            if !generated && !local_hook_policy_v1_supported() {
                println!(
                    "  WARNING: this install uses script hooks, which POST directly and \
                     cannot enforce the mode. Allowlist is stored but NOT in force here."
                );
            }
        }
        // Preserve a project-strategy an earlier `--apply` baked when this run
        // did not pass `--project-strategy`. Without this, a bare re-apply —
        // notably the auto-refresh in `ai-memory upgrade` — re-renders the hook
        // commands with no strategy and silently reverts a `repo-root` install
        // to `basename`. An explicit `--project-strategy` (including `basename`)
        // is honored as-is, so an intentional downgrade still works. Resolving
        // into `args` here means every downstream renderer picks it up with no
        // per-agent plumbing.
        args.project_strategy = install_project_strategy(&args);
        return match args.agent {
            AgentChoice::OpenCode => {
                apply_to_opencode_plugin(&server_url, auth, &args, &capture_mode)
            }
            AgentChoice::OpenCode2 => {
                apply_to_opencode2_plugin(&server_url, auth, &args, &capture_mode)
            }
            AgentChoice::ClaudeCode => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_claude_code_settings(
                    &hooks_dir,
                    &server_url,
                    auth,
                    &config.data_dir,
                    &args,
                )
            }
            AgentChoice::Zcode => apply_to_zcode_hooks(&server_url, auth, &config.data_dir, &args),
        };
    }
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    // Preview must bake the same `CAPTURE_MODE` the next `--apply` would
    // persist (#661) — resolved read-only so a bare preview never writes.
    let preview_capture_mode = resolve_capture_mode(&config.data_dir, args.capture_mode);
    match args.agent {
        AgentChoice::OpenCode => {
            render_opencode_plugin(&server_url, auth, strategy, &preview_capture_mode)
        }
        AgentChoice::OpenCode2 => {
            render_opencode2_plugin(&server_url, auth, strategy, &preview_capture_mode)
        }
        AgentChoice::ClaudeCode => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            let settings_path = match &args.config_file {
                Some(p) => p.clone(),
                None => claude_settings_path()?,
            };
            render_claude_code(
                &hooks_dir,
                &server_url,
                auth,
                &config.data_dir,
                strategy,
                &settings_path,
                ClaudeCaptureScope {
                    assistant: args.capture_assistant,
                    prompts: install_claude_prompt_capture(&args),
                },
            )
        }
        AgentChoice::Zcode => render_zcode(&server_url, auth, &config.data_dir, strategy),
    }
}

#[derive(Debug, Clone, Default)]
struct InferredMcpConfig {
    hook_server_url: Option<String>,
    auth_token: Option<String>,
}

/// The project-strategy to install for `args`: the explicit `--project-strategy`
/// when one was given, otherwise the strategy an earlier `--apply` baked into
/// the agent's existing config (so a bare re-apply preserves it instead of
/// reverting to basename). `None` means "bake nothing", unchanged from before.
fn install_project_strategy(args: &InstallHooksArgs) -> Option<ProjectStrategyArg> {
    if args.project_strategy.is_some() {
        return args.project_strategy;
    }
    existing_agent_config(args)
        .as_deref()
        .and_then(|existing| baked_project_strategy(args.agent, existing))
}

/// The capture failure mode (#446) currently in force: an explicit
/// `--capture-mode` flag wins, otherwise the value stored under `data_dir`
/// from an earlier `--apply`, otherwise the historical `denylist` default.
/// Read-only — used by both [`persist_capture_mode`] and the print-only
/// preview path so a preview's baked `CAPTURE_MODE` matches what the next
/// `--apply` would actually persist (#661).
fn resolve_capture_mode(data_dir: &Path, requested: Option<CaptureModeArg>) -> String {
    let Some(requested) = requested else {
        let path = data_dir.join(crate::commands::hook::CAPTURE_MODE_FILE);
        return match fs::read_to_string(&path) {
            Ok(text) if text.trim().eq_ignore_ascii_case("allowlist") => "allowlist".to_string(),
            _ => "denylist".to_string(),
        };
    };
    match requested {
        CaptureModeArg::Allowlist => "allowlist".to_string(),
        CaptureModeArg::Denylist => "denylist".to_string(),
    }
}

/// Settle and persist the capture failure mode (#446), returning the mode now
/// in force.
///
/// An explicit flag writes the file. Omitting the flag *reads* the stored
/// value rather than defaulting to it, so a bare `--apply` — including the
/// auto-refresh inside `ai-memory upgrade` — can never quietly downgrade an
/// existing opt-in back to capture-by-default.
fn persist_capture_mode(data_dir: &Path, requested: Option<CaptureModeArg>) -> Result<String> {
    let value = resolve_capture_mode(data_dir, requested);
    if requested.is_some() {
        let path = data_dir.join(crate::commands::hook::CAPTURE_MODE_FILE);
        fs::create_dir_all(data_dir)
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;
        // Atomically, because every reader maps an unrecognised value onto
        // `denylist`: a torn write would not corrupt the opt-in, it would
        // silently revert it to capture-by-default (`CONTRIBUTING.md`,
        // "Atomic file writes only").
        ai_memory_wiki::write_atomic(&path, format!("{value}\n").as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(value)
}

/// Whether the Claude Code install should include its prompt-capture hook.
/// Explicit flags win. A bare apply preserves the state of an existing
/// ai-memory install so `upgrade` cannot silently restore a privacy opt-out.
fn install_claude_prompt_capture(args: &InstallHooksArgs) -> bool {
    if args.no_capture_prompts {
        return false;
    }
    if args.capture_prompts || !args.apply {
        return true;
    }
    existing_agent_config(args)
        .as_deref()
        .and_then(baked_claude_prompt_capture)
        .unwrap_or(true)
}

/// Recover prompt-capture state only from hook entries owned by ai-memory.
/// `None` means this is not an existing ai-memory Claude Code install.
fn baked_claude_prompt_capture(existing: &str) -> Option<bool> {
    let document: serde_json::Value = serde_json::from_str(existing).ok()?;
    let hooks = document.get("hooks")?.as_object()?;
    let has_ai_memory_hooks = hooks.values().any(|value| {
        value
            .as_array()
            .is_some_and(|entries| entries.iter().any(is_ai_memory_hook_entry))
    });
    if !has_ai_memory_hooks {
        return None;
    }
    Some(
        hooks
            .get(CLAUDE_PROMPT_EVENT)
            .and_then(serde_json::Value::as_array)
            .is_some_and(|entries| entries.iter().any(is_ai_memory_hook_entry)),
    )
}

/// Read the config file `--apply` will update for the selected agent.
fn existing_agent_config(args: &InstallHooksArgs) -> Option<String> {
    let path = if let Some(path) = &args.config_file {
        path.clone()
    } else {
        match args.agent {
            AgentChoice::ClaudeCode => claude_settings_path().ok()?,
            AgentChoice::OpenCode => opencode_plugin_path().ok()?,
            AgentChoice::OpenCode2 => opencode2_plugin_path().ok()?,
            AgentChoice::Zcode => zcode_config_path().ok()?,
        }
    };
    std::fs::read_to_string(path).ok()
}

/// Recover a strategy only from configuration entries ai-memory owns. Shared
/// JSON/TOML config files may contain unrelated hooks, while generated
/// TypeScript files carry an explicit ownership header.
fn baked_project_strategy(agent: AgentChoice, existing: &str) -> Option<ProjectStrategyArg> {
    match agent {
        AgentChoice::OpenCode | AgentChoice::OpenCode2 => {
            let marker = match agent {
                AgentChoice::OpenCode => "--agent opencode --apply`.",
                AgentChoice::OpenCode2 => "--agent opencode2 --apply`.",
                _ => return None,
            };
            existing
                .lines()
                .next()
                .filter(|line| {
                    line.starts_with("// Auto-generated by `ai-memory install-hooks ")
                        && line.ends_with(marker)
                })
                .and_then(|_| project_strategy_from_text(existing))
        }
        _ => serde_json::from_str(existing)
            .ok()
            .as_ref()
            .and_then(project_strategy_from_json),
    }
}

fn project_strategy_from_json(value: &serde_json::Value) -> Option<ProjectStrategyArg> {
    if is_ai_memory_hook_entry(value) {
        if let Some(strategy) = value
            .get("command")
            .and_then(|command| command.as_str())
            .and_then(project_strategy_from_text)
        {
            return Some(strategy);
        }
        if let Some(args) = value.get("args").and_then(|args| args.as_array()) {
            let args: Vec<&str> = args.iter().filter_map(|arg| arg.as_str()).collect();
            for (index, arg) in args.iter().enumerate() {
                if let Some(value) = arg.strip_prefix("--project-strategy=")
                    && is_repo_root_strategy(value)
                {
                    return Some(ProjectStrategyArg::RepoRoot);
                }
                if *arg == "--project-strategy"
                    && args
                        .get(index + 1)
                        .is_some_and(|value| is_repo_root_strategy(value))
                {
                    return Some(ProjectStrategyArg::RepoRoot);
                }
            }
        }
    }

    match value {
        serde_json::Value::Array(values) => values.iter().find_map(project_strategy_from_json),
        serde_json::Value::Object(values) => values.values().find_map(project_strategy_from_json),
        _ => None,
    }
}

/// Only `repo-root` is baked (`basename` removes the marker), so it is the sole
/// value recovered from legacy shell commands and generated source.
fn project_strategy_from_text(existing: &str) -> Option<ProjectStrategyArg> {
    for marker in [
        "AI_MEMORY_PROJECT_STRATEGY=",
        "--project-strategy=",
        "--project-strategy ",
        "const DEFAULT_PROJECT_STRATEGY =",
    ] {
        for rest in existing.split(marker).skip(1) {
            let token: String = rest
                .trim_start_matches(|character: char| {
                    character.is_ascii_whitespace() || matches!(character, '\'' | '"')
                })
                .chars()
                .take_while(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
                .collect();
            if is_repo_root_strategy(&token) {
                return Some(ProjectStrategyArg::RepoRoot);
            }
        }
    }
    None
}

fn is_repo_root_strategy(value: &str) -> bool {
    matches!(value, "repo-root" | "repo_root")
}

/// Reject `--as-user X` without a usable `--auth-token`. P1.8
/// metadata flag — without a token, the hook scripts would still
/// authenticate anonymously (or as root if the operator reused the
/// config bearer), so the `--as-user X` label would be misleading.
/// Trims whitespace; empty / whitespace-only `--as-user` is treated
/// as not-set so an accidental `--as-user ""` doesn't bail.
///
/// # Errors
/// Returns an error when `as_user` is set but `auth_token` is `None`
/// (or whitespace-only). The error message names the user so
/// operators see which arg they meant to pair with `--auth-token`.
fn validate_as_user(as_user: Option<&str>, auth_token: Option<&str>) -> Result<()> {
    let Some(user) = as_user.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    if auth_token.map(str::trim).is_none_or(str::is_empty) {
        anyhow::bail!(
            "--as-user '{user}' requires --auth-token \
             (issue one with `ai-memory api-key add --username {user} --label <label>`)"
        );
    }
    Ok(())
}

fn effective_hook_server_url(
    config: &Config,
    args: &InstallHooksArgs,
    inferred: Option<&InferredMcpConfig>,
) -> String {
    let raw = if let Some(url) = &args.server_url {
        url.clone()
    } else if config.server_url_configured() {
        config.server_url.clone()
    } else if let Some(url) = inferred.and_then(|mcp| mcp.hook_server_url.clone()) {
        url
    } else {
        return DEFAULT_SERVER_URL.to_string();
    };
    apply_base_path_to_hook_url(&normalise_hook_server_url(&raw), &config.base_path)
}

fn normalise_hook_server_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Thread `Config::base_path` into the URL baked into hook commands so
/// the native hook subcommand (`ai-memory hook`) and the POSIX
/// `.sh`/`.ps1` scripts POST to `<origin><base>/hook` instead of
/// 404'ing under a reverse proxy.
///
/// Skip when the resolved URL already carries a path component — that
/// means the operator put the prefix into `AI_MEMORY_SERVER_URL`
/// directly (`http://host:49374/wiki`) and we'd double it otherwise.
fn apply_base_path_to_hook_url(url: &str, base_path: &str) -> String {
    let (origin, existing_path) = crate::http_client::split_origin_and_path(url);
    if !existing_path.is_empty() {
        return url.to_string();
    }
    let prefix = ai_memory_web::normalize_prefix(base_path);
    if prefix.is_empty() {
        origin
    } else {
        format!("{origin}{prefix}")
    }
}

fn infer_installed_mcp_config(agent: AgentChoice) -> Result<Option<InferredMcpConfig>> {
    let Some(client) = mcp_client_for_agent(agent) else {
        return Ok(None);
    };
    let path = install_mcp::mcp_config_path(client)?;
    let Ok(content) = fs::read_to_string(path) else {
        return Ok(None);
    };
    match client {
        McpClient::ClaudeCode => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "ai-memory"],
            "url",
        )),
        McpClient::OpenCode => Ok(infer_json_mcp_config(
            &content,
            &["mcp", "ai-memory"],
            "url",
        )),
        McpClient::OpenCode2 => Ok(infer_json_mcp_config(
            &content,
            &["mcp", "servers", "ai-memory"],
            "url",
        )),
        McpClient::Zcode => Ok(infer_json_mcp_config(
            &content,
            &["mcp", "servers", "ai-memory"],
            "url",
        )),
        // MCP-only clients never come back from `mcp_client_for_agent`, but
        // the match must stay exhaustive; infer from the standard
        // `mcpServers` map the same way Claude Code does.
        McpClient::Trae | McpClient::WorkBuddy => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "ai-memory"],
            "url",
        )),
    }
}

fn mcp_client_for_agent(agent: AgentChoice) -> Option<McpClient> {
    match agent {
        AgentChoice::ClaudeCode => Some(McpClient::ClaudeCode),
        AgentChoice::OpenCode => Some(McpClient::OpenCode),
        AgentChoice::OpenCode2 => Some(McpClient::OpenCode2),
        // The ZCode MCP client ships with #511; until that lands there is
        // no `McpClient::Zcode` whose config the installer could scrape a
        // server URL or token from.
        AgentChoice::Zcode => None,
    }
}

fn infer_json_mcp_config(
    content: &str,
    entry_path: &[&str],
    url_key: &str,
) -> Option<InferredMcpConfig> {
    let root: serde_json::Value = serde_json::from_str(content).ok()?;
    let mut entry = &root;
    for key in entry_path {
        entry = entry.get(*key)?;
    }
    let hook_server_url = entry
        .get(url_key)
        .and_then(|v| v.as_str())
        .and_then(hook_server_url_from_mcp_url);
    let auth_token = entry
        .get("headers")
        .and_then(|headers| headers.get("Authorization"))
        .and_then(|v| v.as_str())
        .and_then(bearer_token_from_header);
    Some(InferredMcpConfig {
        hook_server_url,
        auth_token,
    })
}

fn hook_server_url_from_mcp_url(url: &str) -> Option<String> {
    // Drop query/fragment BEFORE the `/mcp` peel: Kimi Code's
    // `?flavor=moonshot` marker must never leak into hook URLs (and it
    // would stop the suffix from matching).
    let base = url
        .trim()
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    Some(base.strip_suffix("/mcp").unwrap_or(base).to_string())
}

fn bearer_token_from_header(header: &str) -> Option<String> {
    header
        .trim()
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// True if a hook-array entry belongs to ai-memory — i.e. some command handler
/// inside it is one of our legacy command strings or one of our exec-form native
/// hook handlers. Used to replace our own entries on re-apply while preserving
/// hooks that other tools registered under the same event.
fn is_ai_memory_hook_entry(entry: &serde_json::Value) -> bool {
    fn mentions_ai_memory(value: &serde_json::Value) -> bool {
        let Some(command) = value.get("command").and_then(|c| c.as_str()) else {
            return false;
        };
        let lower = command.to_ascii_lowercase();
        let args = value.get("args").and_then(|a| a.as_array());
        let Some(args) = args else {
            return hook_command_is_ours(command);
        };
        let tokens: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
        // Exec form: require both an ai-memory-ish executable and our hook argv
        // signature so unrelated helpers such as `ai-memory-helper.exe` are not
        // removed just because their executable name contains ai-memory.
        (lower.contains("ai-memory") || lower.contains("ai_memory"))
            && tokens.contains(&"hook")
            && tokens.contains(&"--event")
            && tokens.contains(&"--agent")
            && tokens.contains(&"--server-url")
    }
    // Flat shape (Cursor): `{ "type":"command", "command":"…" }`.
    // Nested shape (Claude Code / Codex / Gemini):
    // `{ "matcher":"", "hooks":[ {"command":"…"} ] }`.
    mentions_ai_memory(entry)
        || entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .is_some_and(|inner| inner.iter().any(mentions_ai_memory))
}

/// Overlay our hook entries for one event onto the user's existing array
/// for that event: drop any prior ai-memory entries (so re-running
/// `install-hooks` never duplicates them) and append ours, while keeping
/// every third-party hook registered under the same event. Replaces a
/// blind `map.insert(event, value)`, which discarded co-located hooks
/// from other tools (e.g. a context-mode SessionStart hook).
fn overlay_event_hooks(
    map: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
    our_value: &serde_json::Value,
) {
    let mut entries: Vec<serde_json::Value> = map
        .get(event)
        .and_then(|v| v.as_array())
        .map(|existing| {
            existing
                .iter()
                .filter(|e| !is_ai_memory_hook_entry(e))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if let Some(ours) = our_value.as_array() {
        entries.extend(ours.iter().cloned());
    }
    map.insert(event.to_string(), serde_json::Value::Array(entries));
}

/// Remove only ai-memory's entries for one event. Delete the event key when
/// no third-party hooks remain so a rendered opt-out stays minimal.
fn remove_ai_memory_event_hooks(map: &mut serde_json::Map<String, serde_json::Value>, event: &str) {
    overlay_event_hooks(map, event, &serde_json::Value::Array(Vec::new()));
    if map
        .get(event)
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        map.remove(event);
    }
}

/// script-fallback platform cannot honor the opt-in, so the installer bails
/// instead of enabling it silently.
fn capture_assistant_allowed(agent: AgentChoice) -> bool {
    matches!(agent, AgentChoice::ClaudeCode) && local_hook_policy_v1_supported()
}

fn prompt_capture_options_allowed(agent: AgentChoice) -> bool {
    agent == AgentChoice::ClaudeCode
}

fn configure_claude_prompt_capture(
    mut payload: serde_json::Value,
    capture_prompts: bool,
) -> serde_json::Value {
    if !capture_prompts
        && let Some(hooks) = payload
            .get_mut("hooks")
            .and_then(serde_json::Value::as_object_mut)
    {
        hooks.remove(CLAUDE_PROMPT_EVENT);
    }
    payload
}

fn apply_to_claude_code_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts(hooks_dir, "claude-code", data_dir)?;
    apply_to_claude_code_settings_with_staged(&staged, server_url, auth_token, data_dir, args)
}

#[cfg(test)]
fn apply_to_claude_code_settings_in(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    staging_data_local: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts_in(hooks_dir, "claude-code", staging_data_local)?;
    let command_dir = staged_command_dir(&staged, "claude-code");
    let capture_prompts = install_claude_prompt_capture(args);
    let payload = configure_claude_prompt_capture(
        crate::commands::render_shared::build_claude_code_script_payload_for_test(
            &command_dir,
            server_url,
            auth_token,
            Some(data_dir),
            args.project_strategy.and_then(ProjectStrategyArg::baked),
            args.capture_assistant,
        ),
        capture_prompts,
    );
    apply_to_claude_code_settings_with_payload(payload, args, capture_prompts)
}

fn apply_to_claude_code_settings_with_staged(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let command_dir = staged_command_dir(staged, "claude-code");
    let capture_prompts = install_claude_prompt_capture(args);
    let payload = configure_claude_prompt_capture(
        build_claude_code_payload_with_data_dir(
            &command_dir,
            server_url,
            auth_token,
            Some(data_dir),
            args.project_strategy.and_then(ProjectStrategyArg::baked),
            args.capture_assistant,
        ),
        capture_prompts,
    );
    apply_to_claude_code_settings_with_payload(payload, args, capture_prompts)
}

fn apply_to_claude_code_settings_with_payload(
    payload: serde_json::Value,
    args: &InstallHooksArgs,
    capture_prompts: bool,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => claude_settings_path()?,
    };
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: build_claude_code_payload didn't return a hooks object")?
        .clone();
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            // Get-or-create the top-level `hooks` table, then merge our
            // event keys in via `overlay_event_hooks`: our entries replace
            // any prior ai-memory entries, while hooks the user (or another
            // tool) wired under the same event — or under a non-overlapping
            // event name (e.g. a hand-written "Notification" hook) — survive.
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in settings.json but not an object")?;
            for (event, value) in &our_hooks {
                overlay_event_hooks(hooks, event, value);
            }
            if !capture_prompts {
                remove_ai_memory_event_hooks(hooks, CLAUDE_PROMPT_EVENT);
            }
            Ok(())
        })
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

/// OpenCode's integration surface is a TypeScript plugin, not a JSON
/// hook table. The plugin posts normalized lifecycle payloads directly
/// to `/hook` and injects pending handoffs through
/// `experimental.chat.system.transform`, because plugin shell stdout is
/// not prepended to the model context the way Claude Code hook stdout is.
fn apply_to_opencode_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    args: &InstallHooksArgs,
    capture_mode: &str,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => opencode_plugin_path()?,
    };
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let body = build_opencode_plugin(server_url, auth_token, strategy, capture_mode);

    let outcome = apply_atomic(&path, move |_existing| Ok(body.clone()))?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new plugin file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    if !matches!(outcome, ApplyOutcome::NoOp) {
        println!();
        println!("OpenCode auto-loads plugins from ~/.config/opencode/plugins/ on next start.");
        println!("If you're already inside an `opencode` session, restart it for the");
        println!("new plugin to take effect.");
    }
    Ok(())
}

fn render_opencode_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    capture_mode: &str,
) -> Result<()> {
    println!("// OpenCode plugin — write to ~/.config/opencode/plugins/ai-memory.ts");
    println!("// Or re-run with `--apply` to install it automatically.");
    println!("// Restart OpenCode after changing plugins; config is loaded at startup.");
    println!();
    println!(
        "{}",
        build_opencode_plugin(server_url, auth_token, project_strategy, capture_mode)
    );
    Ok(())
}

/// `~/.config/opencode/plugins/ai-memory-opencode2.ts` — the OpenCode 2.0
/// beta plugin file. The beta shares v1's config dir and session store but
/// not its plugin API, so this is a distinct file from `ai-memory.ts`
/// (uninstall keys each file to its own banner + agent constant).
pub(crate) fn opencode2_plugin_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.config/opencode")?
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("ai-memory-opencode2.ts"))
}

/// Generate an OpenCode 2.0 beta plugin at
/// `~/.config/opencode/plugins/ai-memory-opencode2.ts`.
///
/// The beta's plugin API (`Plugin.define({ id, setup })` with
/// `ctx.session.hook` / `ctx.tool.hook` / `ctx.event.subscribe`) is
/// incompatible with v1's function plugin, so the beta gets its own file.
/// Both files share the one auto-loaded dir while the beta is side-by-side;
/// a host may warn about its sibling's file (API mismatch) — that warning
/// is benign, and `uninstall` removes each file only on its own ownership
/// markers.
fn apply_to_opencode2_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    args: &InstallHooksArgs,
    capture_mode: &str,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => opencode2_plugin_path()?,
    };
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let body = build_opencode2_plugin(server_url, auth_token, strategy, capture_mode)?;

    let outcome = apply_atomic(&path, move |_existing| Ok(body.clone()))?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new plugin file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    if !matches!(outcome, ApplyOutcome::NoOp) {
        println!();
        println!("OpenCode 2 auto-loads plugins from ~/.config/opencode/plugins/ on next start.");
        println!("If you're already inside an `opencode2` session, restart it for the");
        println!("new plugin to take effect.");
    }
    Ok(())
}

fn render_opencode2_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    capture_mode: &str,
) -> Result<()> {
    println!(
        "// OpenCode 2.0 beta plugin — write to ~/.config/opencode/plugins/ai-memory-opencode2.ts"
    );
    println!("// Or re-run with `--apply` to install it automatically.");
    println!("// Restart OpenCode 2 after changing plugins; config is loaded at startup.");
    println!();
    println!(
        "{}",
        build_opencode2_plugin(server_url, auth_token, project_strategy, capture_mode)?
    );
    Ok(())
}

/// Build the beta plugin from the v1 template. The capture prelude (hook
/// queue + spooling, marker resolution, capture policy, session maps,
/// `/hook` + `/handoff` helpers) is host-independent and reused verbatim;
/// only the host binding is rewritten for the V2 API. Anchors are plain
/// rendered-TS constants like the spool patch's: a template edit that moves
/// them fails loudly here instead of shipping a half-v1 plugin.
fn build_opencode2_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    capture_mode: &str,
) -> Result<String> {
    let v1 = build_opencode_plugin(server_url, auth_token, project_strategy, capture_mode);
    const BANNER_V1: &str =
        "// Auto-generated by `ai-memory install-hooks --agent opencode --apply`.";
    const BANNER_V2: &str =
        "// Auto-generated by `ai-memory install-hooks --agent opencode2 --apply`.";
    const AGENT_V1: &str = "const AGENT = \"open-code\";";
    const AGENT_V2: &str = "const AGENT = \"opencode2\";";
    // Start of the v1 host binding through end of file. Everything before
    // this line (including the spooled delivery path) is shared.
    const V1_BINDING_ANCHOR: &str =
        "\nexport const AiMemoryHooks: Plugin = async ({ directory }) => {";
    // `replacen` silently keeps the v1 text when its anchor moves, which
    // would ship a half-v1 plugin — fail loudly instead, like the spool
    // patch's anchors do.
    let out = replace_opencode_anchor(&v1, BANNER_V1, BANNER_V2, "banner")?;
    let out = replace_opencode_anchor(&out, AGENT_V1, AGENT_V2, "agent constant")?;
    let Some(binding_at) = out.find(V1_BINDING_ANCHOR) else {
        anyhow::bail!("opencode2 template drifted: v1 host binding anchor not found");
    };
    let mut rebuilt = out[..binding_at].to_string();
    rebuilt.push_str(OPENCODE2_BINDING);
    Ok(rebuilt)
}

fn replace_opencode_anchor(haystack: &str, from: &str, to: &str, what: &str) -> Result<String> {
    if !haystack.contains(from) {
        anyhow::bail!("opencode2 template drifted: v1 {what} anchor not found");
    }
    Ok(haystack.replacen(from, to, 1))
}

/// V2 host binding for the shared capture prelude: the beta's `{ id, setup }`
/// plugin shape with its session/tool/event hooks (verified against
/// `@opencode-ai/plugin@beta`, including a `tsc --noEmit` pass over the
/// rendered file). Lifecycle arrives on `ctx.event.subscribe` whose
/// envelopes carry `{ type, data, location }` (v1's `event.properties`
/// is kept as a fallback because the beta schema is still changing).
/// Handoff injection moved from v1's removed
/// `experimental.chat.system.transform` to the `context` hook, which edits
/// the outgoing model call without persisting into history — guarded to
/// inject once per session because it fires on every continuation.
const OPENCODE2_BINDING: &str = r#"
// `Plugin.define` is an identity wrapper, so the binding exports the
// `{ id, setup }` shape directly and keeps the shared `import type` line:
// a runtime import of `@opencode-ai/plugin` does not resolve from the
// global plugins dir and fails the load.
const AiMemoryOpencode2: Plugin = {
  id: "ai-memory-opencode2",
  setup: async (ctx) => {
    const ctxAny = ctx as any;
    const directory = ctxAny?.location?.directory;
    const controller = new AbortController();
    void (async () => {
      try {
        for await (const evt of ctx.event.subscribe({ signal: controller.signal })) {
          const event = evt as any;
          const type = event?.type;
          const data = event?.data ?? event?.properties ?? {};
          const info = data?.info ?? {};
          const loc = data?.location?.directory ?? data?.directory
            ?? event?.location?.directory ?? directory;
          if (type === "session.created") {
            const id = data?.sessionID ?? data?.id ?? info?.id;
            startSession(id, data?.location?.directory ?? loc, {
              title: data?.title ?? info?.title,
              projectID: data?.projectID ?? info?.projectID,
            });
          }
          if (type === "session.idle") {
            const id = data?.sessionID ?? data?.id;
            startSession(id, cwdFor(id, loc));
            postHook("stop", { sessionID: id, cwd: cwdFor(id, loc) });
          }
          if (type === "session.deleted") {
            const id = data?.sessionID ?? data?.id ?? info?.id;
            endSession(id, loc, data?.directory ?? info?.directory);
          }
          if (type === "session.compaction.started") {
            const id = data?.sessionID ?? data?.id;
            postPreCompact(id, loc);
          }
          if (type === "session.compacted") {
            const id = data?.sessionID ?? data?.id;
            postPreCompact(id, loc);
          }
        }
      } catch (_e) {
        // The stream ends on unload. Capture is best-effort and must never
        // break the host.
      }
    })();
    const promptRegistration = await ctx.session.hook("prompt", (event) => {
      const e = event as any;
      const id = e?.sessionID;
      const prompt = e?.prompt ?? {};
      const cwd = cwdFor(id, directory);
      startSession(id, cwd, { agent: prompt?.agent, model: prompt?.model });
      postHook("user-prompt", {
        sessionID: id,
        cwd,
        agent: prompt?.agent,
        model: prompt?.model,
        messageID: e?.messageID,
        prompt: typeof prompt?.text === "string" ? prompt.text : textFromParts(prompt?.parts),
      });
    });
    const handoffInjected = new Set<string>();
    const contextRegistration = await ctx.session.hook("context", async (event) => {
      const e = event as any;
      const id = e?.sessionID;
      if (!id || handoffInjected.has(id)) return;
      startSession(id, cwdFor(id, directory));
      let pending = handoffFetches.get(id);
      if (!pending) {
        pending = fetchHandoff(cwdFor(id, directory), id);
        handoffFetches.set(id, pending);
      }
      const handoff = await pending;
      if (handoff) {
        // SystemPart is `{ type: "text", text }` — the `type` discriminator
        // is required: without it the host fails the model call with a
        // schema validation error (observed live on beta-18999).
        e.system.push({ type: "text", text: handoff });
        handoffInjected.add(id);
      }
    });
    const beforeRegistration = await ctx.tool.hook("execute.before", (event) => {
      const e = event as any;
      const id = e?.sessionID;
      startSession(id, cwdFor(id, directory));
      postHook("pre-tool-use", {
        sessionID: id,
        cwd: cwdFor(id, directory),
        tool: e?.tool,
        callID: e?.id,
        args: e?.input,
      });
    });
    const afterRegistration = await ctx.tool.hook("execute.after", (event) => {
      const e = event as any;
      const id = e?.sessionID;
      startSession(id, cwdFor(id, directory));
      // The beta reports failures on the same channel (`status: "error"`,
      // no `result`); keep the message where v1 kept output so the
      // failure reason survives consolidation.
      const failed = e?.status === "error";
      postHook("post-tool-use", {
        sessionID: id,
        cwd: cwdFor(id, directory),
        tool: e?.tool,
        callID: e?.id,
        args: e?.input,
        title: failed ? undefined : e?.result?.title,
        output: failed
          ? String(e?.error?.message ?? e?.error ?? "tool failed")
          : e?.result?.output,
        metadata: failed ? undefined : e?.result?.metadata,
      });
    });
    return async () => {
      controller.abort();
      for (const registration of [
        promptRegistration,
        contextRegistration,
        beforeRegistration,
        afterRegistration,
      ]) {
        try {
          await registration.dispose();
        } catch (_e) {
          // Unload is best-effort; a dead host has nothing to unregister.
        }
      }
      for (const id of Array.from(startedSessions)) {
        endSession(id, directory);
      }
      await drainHookQueueForDispose();
    };
  },
};

export default AiMemoryOpencode2;
"#;
/// `findSettingsMarker` mirrors the native `find_settings_marker` (`marker.rs`)
/// and the shell `ai_memory_find_settings_marker` (`hooks/_lib.sh`), #668:
/// the same ancestor walk and HOME/`.git` boundary as `findMarker`, but a
/// marker that declares nothing beyond `[capture]` is transparent — the walk
/// skips it and continues to the next ancestor. That keeps a nested
/// capture-only marker (e.g. one that only sets `ignore_paths`) from
/// shadowing an outer marker's `workspace`/`project`/etc, without changing
/// `[capture]`/`ignore_paths` resolution itself (that stays on `findMarker`,
/// the nearest marker, in the separate capture-policy template). The
/// boundary walk is duplicated from `findMarker` rather than shared, on
/// purpose: `findMarker` stays untouched, well-exercised, nearest-marker
/// behavior for every other caller.
pub(crate) const TS_FIND_SETTINGS_MARKER: &str = r#"function declaresSettings(text: string): boolean {
  for (const key of ["workspace", "project", "project_strategy", "drop_subagent_captures"]) {
    if (tomlKey(text, key) !== undefined) return true;
  }
  for (const key of ["default_global", "inject_on_session_start", "max_chars"]) {
    if (tomlFlag(text, key) !== undefined) return true;
  }
  return false;
}

function findSettingsMarker(cwd: string | undefined): string | undefined {
  if (!cwd) return undefined;
  let dir = resolve(cwd);
  const home = homedir();
  let boundary: string | undefined;
  if (home && (dir === home || dir.startsWith(home.endsWith(sep) ? home : home + sep))) {
    boundary = home;
  } else if (home) {
    let probe = dir;
    while (probe && probe !== dirname(probe)) {
      if (existsSync(join(probe, ".git"))) {
        boundary = probe;
        break;
      }
      probe = dirname(probe);
    }
    boundary ??= dir;
  }
  while (dir && dir !== dirname(dir)) {
    const marker = join(dir, ".ai-memory.toml");
    if (existsSync(marker)) {
      try {
        if (declaresSettings(readFileSync(marker, "utf8"))) return marker;
      } catch (_e) {
      }
    }
    if (boundary && dir === boundary) return undefined;
    dir = dirname(dir);
  }
  return undefined;
}"#;

/// Emit the `applyMarkerParams` TypeScript function shared verbatim by the
/// OpenCode plugin and the OMP extension.
///
/// `None` reproduces the historical marker-only function byte-for-byte, so
/// existing generated files and golden tests are unchanged. `Some(default)`
/// prepends a `DEFAULT_PROJECT_STRATEGY` const and emits a variant that applies
/// that install-time default when no marker pins a `project_strategy` (#128).
/// A marker's own `project` / `project_strategy` still take precedence (§3.3),
/// and repo-root is resolved host-side via `repoRootProject`.
///
/// Scope/settings resolution walks past a capture-only marker to the nearest
/// ancestor marker that declares a setting (#668) via `findSettingsMarker`,
/// emitted alongside this function.
fn ts_apply_marker_params(default_strategy: Option<&str>) -> String {
    let Some(default) = default_strategy else {
        return format!(
            "{TS_TOML_FLAG}\n{TS_FIND_SETTINGS_MARKER}\n{}",
            r#"function applyMarkerParams(url: URL, cwd: string | undefined): void {
  const managedRun = process.env.AI_MEMORY_RUN_ID;
  if (managedRun) url.searchParams.set("managed_run", managedRun);
  const marker = findSettingsMarker(cwd);
  if (!marker || !cwd) return;
  url.searchParams.set("cwd", cwd);
  try {
    const body = readFileSync(marker, "utf8");
    const workspace = tomlKey(body, "workspace");
    const project = tomlKey(body, "project");
    const projectStrategy = tomlKey(body, "project_strategy");
    const dropSubagent = tomlKey(body, "drop_subagent_captures");
    const defaultGlobal = tomlFlag(body, "default_global");
    const briefing = tomlFlag(body, "inject_on_session_start");
    const briefingBudget = tomlFlag(body, "max_chars");
    if (workspace) url.searchParams.set("workspace", workspace);
    if (project) url.searchParams.set("project", project);
    if (projectStrategy) url.searchParams.set("project_strategy", projectStrategy);
    if (dropSubagent) url.searchParams.set("drop_subagent", dropSubagent);
    if (defaultGlobal) url.searchParams.set("default_global", defaultGlobal);
    if (briefing) url.searchParams.set("briefing", briefing);
    if (briefingBudget) url.searchParams.set("briefing_budget", briefingBudget);
    if (!project && (projectStrategy === "repo-root" || projectStrategy === "repo_root")) {
      const repoProject = repoRootProject(cwd);
      if (repoProject) url.searchParams.set("project", repoProject);
    }
  } catch (_e) {
  }
}"#
        );
    };
    let body = r#"function applyMarkerParams(url: URL, cwd: string | undefined): void {
  const managedRun = process.env.AI_MEMORY_RUN_ID;
  if (managedRun) url.searchParams.set("managed_run", managedRun);
  if (!cwd) return;
  url.searchParams.set("cwd", cwd);
  let workspace: string | undefined;
  let project: string | undefined;
  let projectStrategy: string | undefined;
  let dropSubagent: string | undefined;
  let defaultGlobal: string | undefined;
  let briefing: string | undefined;
  let briefingBudget: string | undefined;
  const marker = findSettingsMarker(cwd);
  if (marker) {
    try {
      const body = readFileSync(marker, "utf8");
      workspace = tomlKey(body, "workspace");
      project = tomlKey(body, "project");
      projectStrategy = tomlKey(body, "project_strategy");
      dropSubagent = tomlKey(body, "drop_subagent_captures");
      defaultGlobal = tomlFlag(body, "default_global");
      briefing = tomlFlag(body, "inject_on_session_start");
      briefingBudget = tomlFlag(body, "max_chars");
    } catch (_e) {
    }
  }
  if (!projectStrategy) projectStrategy = DEFAULT_PROJECT_STRATEGY;
  if (!project && (projectStrategy === "repo-root" || projectStrategy === "repo_root")) {
    const repoProject = repoRootProject(cwd);
    if (repoProject) project = repoProject;
  }
  if (workspace) url.searchParams.set("workspace", workspace);
  if (project) url.searchParams.set("project", project);
  if (projectStrategy) url.searchParams.set("project_strategy", projectStrategy);
  if (dropSubagent) url.searchParams.set("drop_subagent", dropSubagent);
  if (defaultGlobal) url.searchParams.set("default_global", defaultGlobal);
  if (briefing) url.searchParams.set("briefing", briefing);
  if (briefingBudget) url.searchParams.set("briefing_budget", briefingBudget);
}"#;
    format!(
        "const DEFAULT_PROJECT_STRATEGY = {};\n{TS_TOML_FLAG}\n{TS_FIND_SETTINGS_MARKER}\n{body}",
        ts_string_literal(default)
    )
}

/// `tomlFlag` mirrors the native hook's `parse_toml_flag`: unlike `tomlKey`
/// (quoted strings only) it also accepts a bare token (`default_global =
/// true`, `max_chars = 4000`), so section-style marker keys work whether or
/// not the operator quotes the value. Emitted next to `applyMarkerParams`
/// in every generated TypeScript integration.
pub(crate) const TS_TOML_FLAG: &str = r#"function tomlFlag(text: string, key: string): string | undefined {
  const re = new RegExp(`^\\s*${key}\\s*=\\s*(?:"([^"]*)"|([^#\\s]+))`);
  for (const line of text.split(/\r?\n/)) {
    const match = re.exec(line);
    if (match) return match[1] ?? match[2];
  }
  return undefined;
}"#;

/// Post-process a generated TypeScript integration so failed hook
/// deliveries SPOOL instead of vanishing (#580). The shell hooks have
/// spooled since day one; the TS plugins fire-and-forgot, so a laptop
/// off the server's network lost whole sessions of capture. Entries are
/// written in the CLI spool's exact on-disk contract
/// (`{ms:013}-{pid}-{seq:016x}.json`, `SpoolEntry` fields, 0700 dir /
/// 0600 files, tmp+rename) so `ai-memory hook-drain` and the shell
/// hooks' piggyback drain deliver them too; the plugin also drains its
/// own spool on first use and after any queue flush. An `ingest_key`
/// is minted at spool time, so a double drain (plugin + CLI) dedupes
/// server-side instead of double-ingesting.
///
/// Implemented as a transformation over the rendered source (rather
/// than another copy of the template) so all TS integrations share one
/// audited implementation; it fails loudly if the delivery block it
/// patches ever drifts.
fn add_hook_spooling(source: String) -> Result<String> {
    const FETCH_BLOCK: &str = r#"      try {
        await fetch(item.url, {
          method: "POST",
          headers: { "Content-Type": "application/json", ...authHeaders() },
          body: JSON.stringify(item.payload),
          signal: timeoutSignal(HOOK_REQUEST_TIMEOUT_MS),
        }).catch(() => undefined);
      } catch (_e) {
        // Best-effort capture. Hooks must never block the agent.
      }"#;
    const FETCH_REPLACEMENT: &str = r#"      try {
        const resp = await fetch(item.url, {
          method: "POST",
          headers: { "Content-Type": "application/json", ...authHeaders() },
          body: JSON.stringify(item.payload),
          signal: timeoutSignal(HOOK_REQUEST_TIMEOUT_MS),
        }).catch(() => undefined);
        // Unreachable server or 5xx: keep the event on disk for a later
        // drain (#580). 4xx is permanent - spooling it would retry a
        // rejection forever.
        if (!resp || resp.status >= 500) spoolFailedHook(item.url, item.payload);
      } catch (_e) {
        try { spoolFailedHook(item.url, item.payload); } catch (_e2) {}
      }"#;
    const ENQUEUE_ANCHOR: &str = "function enqueueHook(";
    let runtime = crate::commands::render_shared::ts_spool_runtime();
    let resolve_fn = crate::commands::render_shared::ts_resolve_token_fn();
    if !source.contains(FETCH_BLOCK) {
        anyhow::bail!(
            "TS integration template drifted: the hook delivery block the \
             spool patch targets was not found"
        );
    }
    if source.matches(ENQUEUE_ANCHOR).count() != 1 {
        anyhow::bail!("TS integration template drifted: enqueueHook anchor not unique");
    }
    let mut out = source.replacen(FETCH_BLOCK, FETCH_REPLACEMENT, 1);
    out = out.replacen(
        ENQUEUE_ANCHOR,
        &format!("{resolve_fn}{runtime}{ENQUEUE_ANCHOR}"),
        1,
    );
    // Drain the offline spool alongside every queue flush, and extend the
    // imports the runtime needs.
    let drain_anchor =
        "async function drainHookQueue(): Promise<void> {\n  if (hookDraining) return;";
    if out.matches(drain_anchor).count() != 1 {
        anyhow::bail!("TS integration template drifted: drainHookQueue anchor not unique");
    }
    out = out.replacen(
        drain_anchor,
        "async function drainHookQueue(): Promise<void> {\n  if (hookDraining) return;\n  requestSpoolDrain();",
        1,
    );
    let import_anchor = "import { closeSync, existsSync, openSync, readFileSync as readMarkerText, readSync } from \"node:fs\";";
    if out.matches(import_anchor).count() != 1 {
        anyhow::bail!("TS integration template drifted: node:fs import anchor not unique");
    }
    out = out.replacen(
        import_anchor,
        "import { closeSync, existsSync, mkdirSync, openSync, readFileSync as readMarkerText, readSync, readdirSync, renameSync, unlinkSync, writeFileSync } from \"node:fs\";",
        1,
    );
    Ok(out)
}

fn build_opencode_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    capture_mode: &str,
) -> String {
    let token_line = auth_token
        .map(|t| format!("const TOKEN: string | null = {};\n", ts_string_literal(t)))
        .unwrap_or_else(|| "const TOKEN: string | null = null;\n".to_string());
    let apply_marker_params = ts_apply_marker_params(project_strategy);
    let capture_policy = ts_capture_policy_v1(capture_mode);
    let body = format!(
        r#"// Auto-generated by `ai-memory install-hooks --agent opencode --apply`.
// Edit by re-running the command, not by hand — install-hooks
// will overwrite this file (with a `.bak-<ts>` backup) on each
// re-run.

import type {{ Plugin }} from "@opencode-ai/plugin";
import {{ execFileSync }} from "node:child_process";
import {{ closeSync, existsSync, openSync, readFileSync as readMarkerText, readSync }} from "node:fs";
import {{ basename, dirname, join, resolve, sep }} from "node:path";
import {{ homedir }} from "node:os";

const SERVER = {server_literal}.replace(/\/+$/, "");
const AGENT = "open-code";
{token_line}
{capture_policy}

function timeoutSignal(ms: number): AbortSignal | undefined {{
  if (typeof AbortSignal === "undefined") return undefined;
  const factory = (AbortSignal as unknown as {{ timeout?: (ms: number) => AbortSignal }}).timeout;
  return factory ? factory(ms) : undefined;
}}

function authHeaders(): Record<string, string> {{
  const token = resolveToken();
  return token ? {{ Authorization: `Bearer ${{token}}` }} : {{}};
}}

const HOOK_QUEUE_MAX = 100;
const HOOK_FLUSH_INTERVAL_MS = 2000;
const HOOK_FLUSH_THRESHOLD = 20;
const HOOK_INTER_REQUEST_DELAY_MS = 50;
const HOOK_REQUEST_TIMEOUT_MS = 2000;
const HOOK_DISPOSE_DRAIN_BUDGET_MS = 2000;
const HOOK_IMMEDIATE_EVENTS = new Set(["session-start", "stop", "session-end", "pre-compact"]);

type HookQueueItem = {{ event: string; url: URL; payload: Record<string, unknown> }};
const hookQueue: HookQueueItem[] = [];
let hookFlushTimer: ReturnType<typeof setTimeout> | undefined;
let hookDraining = false;
let hookDrainPromise: Promise<void> | undefined;

function sleep(ms: number): Promise<void> {{
  return new Promise((resolve) => setTimeout(resolve, ms));
}}

function scheduleHookFlush(): void {{
  if (hookFlushTimer) return;
  hookFlushTimer = setTimeout(() => {{
    hookFlushTimer = undefined;
    void requestHookDrain();
  }}, HOOK_FLUSH_INTERVAL_MS);
  hookFlushTimer.unref?.();
}}

function requestHookDrain(): Promise<void> {{
  if (!hookDrainPromise) {{
    hookDrainPromise = drainHookQueue().finally(() => {{
      hookDrainPromise = undefined;
      if (hookQueue.length > 0) void requestHookDrain();
    }});
  }}
  return hookDrainPromise;
}}

function disposeDrainTimeout(): Promise<void> {{
  return new Promise((resolve) => {{
    const timer = setTimeout(resolve, HOOK_DISPOSE_DRAIN_BUDGET_MS);
    timer.unref?.();
  }});
}}

async function drainHookQueueForDispose(): Promise<void> {{
  await Promise.race([requestHookDrain(), disposeDrainTimeout()]);
}}

function enqueueHook(event: string, url: URL, payload: Record<string, unknown>): void {{
  if (hookQueue.length >= HOOK_QUEUE_MAX) hookQueue.shift();
  hookQueue.push({{ event, url, payload }});
  if (HOOK_IMMEDIATE_EVENTS.has(event) || hookQueue.length >= HOOK_FLUSH_THRESHOLD) {{
    void requestHookDrain();
  }} else {{
    scheduleHookFlush();
  }}
}}

async function drainHookQueue(): Promise<void> {{
  if (hookDraining) return;
  hookDraining = true;
  if (hookFlushTimer) {{
    clearTimeout(hookFlushTimer);
    hookFlushTimer = undefined;
  }}
  try {{
    while (hookQueue.length > 0) {{
      const item = hookQueue.shift();
      if (!item) break;
      try {{
        await fetch(item.url, {{
          method: "POST",
          headers: {{ "Content-Type": "application/json", ...authHeaders() }},
          body: JSON.stringify(item.payload),
          signal: timeoutSignal(HOOK_REQUEST_TIMEOUT_MS),
        }}).catch(() => undefined);
      }} catch (_e) {{
        // Best-effort capture. Hooks must never block the agent.
      }}
      if (hookQueue.length > 0) await sleep(HOOK_INTER_REQUEST_DELAY_MS);
    }}
  }} finally {{
    hookDraining = false;
  }}
}}

function findMarker(cwd: string | undefined): string | undefined {{
  if (!cwd) return undefined;
  let dir = resolve(cwd);
  const home = homedir();
  let boundary: string | undefined;
  if (home && (dir === home || dir.startsWith(home.endsWith(sep) ? home : home + sep))) {{
    boundary = home;
  }} else if (home) {{
    let probe = dir;
    while (probe && probe !== dirname(probe)) {{
      if (existsSync(join(probe, ".git"))) {{
        boundary = probe;
        break;
      }}
      probe = dirname(probe);
    }}
    boundary ??= dir;
  }}
  while (dir && dir !== dirname(dir)) {{
    const marker = join(dir, ".ai-memory.toml");
    if (existsSync(marker)) return marker;
    if (boundary && dir === boundary) return undefined;
    dir = dirname(dir);
  }}
  return undefined;
}}

function tomlKey(text: string, key: string): string | undefined {{
  const re = new RegExp(`^\\s*${{key}}\\s*=\\s*"([^"]*)"`);
  for (const line of text.split(/\r?\n/)) {{
    const match = re.exec(line);
    if (match) return match[1];
  }}
  return undefined;
}}


function repoRootProject(cwd: string | undefined): string | undefined {{
  if (!cwd) return undefined;
  try {{
    const inside = execFileSync("git", ["-C", cwd, "rev-parse", "--is-inside-work-tree"], {{
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }}).trim();
    if (inside !== "true") return undefined;
    const common = execFileSync("git", ["-C", cwd, "rev-parse", "--path-format=absolute", "--git-common-dir"], {{
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }}).trim();
    if (!common) return undefined;
    const root = dirname(common);
    if (!root || root === dirname(root)) return undefined;
    return basename(root);
  }} catch (_e) {{
    return undefined;
  }}
}}
{apply_marker_params}

function sessionID(input: unknown): string | undefined {{
  const value = input as any;
  return value?.sessionID ?? value?.sessionId ?? value?.session_id ?? value?.info?.id;
}}

function textFromParts(parts: unknown): string {{
  if (!Array.isArray(parts)) return "";
  return parts
    .map((part: any) => {{
      if (part?.type === "text" && typeof part.text === "string") return part.text;
      if (part?.type === "subtask" && typeof part.prompt === "string") return part.prompt;
      if (part?.type === "file" && typeof part.filename === "string") return `[file: ${{part.filename}}]`;
      return "";
    }})
    .filter(Boolean)
    .join("\n\n")
    .trim();
}}

const sessionCwds = new Map<string, string>();
const startedSessions = new Set<string>();
const handoffFetches = new Map<string, Promise<string | undefined>>();
const preCompactLast = new Map<string, number>();

function cwdFor(id: string | undefined, directory: string): string {{
  return (id && sessionCwds.get(id)) || directory;
}}

function rememberCwd(id: string | undefined, cwd: string | undefined): void {{
  if (id && cwd) sessionCwds.set(id, cwd);
}}

function startSession(id: string | undefined, cwd: string, extra: Record<string, unknown> = {{}}): void {{
  if (!id || startedSessions.has(id)) return;
  startedSessions.add(id);
  rememberCwd(id, cwd);
  // Generated integrations inject through fetchHandoff below. In managed mode
  // a queued SessionStart response is not model-visible and must not consume
  // the workstream context before that synchronous fetch receives it.
  if (!process.env.AI_MEMORY_RUN_ID) {{
    postHook("session-start", {{ sessionID: id, cwd, ...extra }});
  }}
}}

function endSession(id: string | undefined, directory: string, cwd?: string): void {{
  if (!id || !startedSessions.delete(id)) return;
  const resolvedCwd = cwd || cwdFor(id, directory);
  postHook("session-end", {{ sessionID: id, cwd: resolvedCwd }});
  sessionCwds.delete(id);
  handoffFetches.delete(id);
  preCompactLast.delete(id);
}}

function postPreCompact(id: string | undefined, directory: string): void {{
  startSession(id, cwdFor(id, directory));
  const key = id || "unknown";
  const now = Date.now();
  const last = preCompactLast.get(key) ?? 0;
  if (now - last < 1000) return;
  preCompactLast.set(key, now);
  postHook("pre-compact", {{ sessionID: id, cwd: cwdFor(id, directory) }});
}}

function postHook(event: string, payload: Record<string, unknown>): void {{
  const url = new URL(`${{SERVER}}/hook`);
  url.searchParams.set("event", event);
  url.searchParams.set("agent", AGENT);
  applyMarkerParams(url, typeof payload.cwd === "string" ? payload.cwd : undefined);
  const policy = capturePolicy(payload, typeof payload.cwd === "string" ? payload.cwd : undefined);
  if (policy.disposition === "drop") return;
  try {{
    enqueueHook(event, url, policy.payload);
  }} catch (_e) {{
    // Best-effort capture. Hooks must never block the agent.
  }}
}}

async function fetchHandoff(cwd: string, id: string | undefined): Promise<string | undefined> {{
  const url = new URL(`${{SERVER}}/handoff`);
  url.searchParams.set("agent", AGENT);
  url.searchParams.set("cwd", cwd);
  if (id) url.searchParams.set("session_id", id);
  applyMarkerParams(url, cwd);
  try {{
    const response = await fetch(url, {{
      headers: authHeaders(),
      signal: timeoutSignal(1000),
    }});
    if (!response.ok) return undefined;
    const text = (await response.text()).trim();
    return text.length > 0 ? text : undefined;
  }} catch (_e) {{
    return undefined;
  }}
}}

export const AiMemoryHooks: Plugin = async ({{ directory }}) => {{
  return {{
    dispose: async () => {{
      for (const id of Array.from(startedSessions)) {{
        endSession(id, directory);
      }}
      await drainHookQueueForDispose();
    }},
    event: async (input) => {{
      const event = (input as any).event;
      const properties = event?.properties ?? {{}};
      if (event?.type === "session.created") {{
        const info = properties.info ?? {{}};
        const id = properties.sessionID ?? info.id;
        const cwd = info.directory ?? directory;
        startSession(id, cwd, {{
          title: info.title,
          projectID: info.projectID,
        }});
      }}
      if (event?.type === "session.idle") {{
        const id = properties.sessionID;
        startSession(id, cwdFor(id, directory));
        postHook("stop", {{ sessionID: id, cwd: cwdFor(id, directory) }});
      }}
      if (event?.type === "session.deleted") {{
        const info = properties.info ?? {{}};
        const id = properties.sessionID ?? info.id;
        endSession(id, directory, info.directory);
      }}
      if (event?.type === "session.compacted") {{
        const id = properties.sessionID;
        postPreCompact(id, directory);
      }}
    }},
    "chat.message": async (input, output) => {{
      const id = sessionID(input);
      const cwd = cwdFor(id, directory);
      startSession(id, cwd, {{ agent: (input as any).agent, model: (input as any).model }});
      postHook("user-prompt", {{
        sessionID: id,
        cwd,
        agent: (input as any).agent,
        model: (input as any).model,
        messageID: (input as any).messageID,
        prompt: textFromParts((output as any).parts),
      }});
    }},
    "tool.execute.before": async (input, output) => {{
      const id = sessionID(input);
      startSession(id, cwdFor(id, directory));
      postHook("pre-tool-use", {{
        sessionID: id,
        cwd: cwdFor(id, directory),
        tool: (input as any).tool,
        callID: (input as any).callID,
        args: (output as any).args,
      }});
    }},
    "tool.execute.after": async (input, output) => {{
      const id = sessionID(input);
      startSession(id, cwdFor(id, directory));
      postHook("post-tool-use", {{
        sessionID: id,
        cwd: cwdFor(id, directory),
        tool: (input as any).tool,
        callID: (input as any).callID,
        args: (input as any).args,
        title: (output as any).title,
        output: (output as any).output,
        metadata: (output as any).metadata,
      }});
    }},
    "experimental.session.compacting": async (input) => {{
      const id = sessionID(input);
      postPreCompact(id, directory);
    }},
    "experimental.chat.system.transform": async (input, output) => {{
      const id = sessionID(input);
      if (!id) return;
      startSession(id, cwdFor(id, directory));
      let pending = handoffFetches.get(id);
      if (!pending) {{
        pending = fetchHandoff(cwdFor(id, directory), id);
        handoffFetches.set(id, pending);
      }}
      const handoff = await pending;
      if (handoff) (output as any).system.push(handoff);
    }},
  }};
}};

export default AiMemoryHooks;
"#,
        server_literal = ts_string_literal(server_url),
        token_line = token_line,
    );
    // Anchors are compile-time constants over this same file's template;
    // a mismatch is a template edit that forgot the spool patch.
    add_hook_spooling(body).expect("generated TS integration lost its hook-spool anchors")
}

/// Warn when Pi and OMP resolve to the same extensions directory.
///
/// Agent-distinct filenames stop the two installs from overwriting each
/// other, but they do not make sharing a directory safe. OMP discovers
/// **every** direct `*.ts` under its extensions directory, so with both
/// files present each agent loads both extensions — and since the Pi
/// extension is the OMP one with `const AGENT` swapped, and neither guards
/// on its host, every lifecycle event is captured twice under two different
/// agent identities.
///
/// That is not a regression this installer can fix by itself: both agents
/// honour the same `PI_CODING_AGENT_DIR`, so the collision is upstream. What
/// it can do is refuse to be silent about it, and name the two ways out.
fn stage_hook_scripts(source_dir: &Path, agent_label: &str, data_dir: &Path) -> Result<PathBuf> {
    stage_hook_scripts_in(
        source_dir,
        agent_label,
        &hook_staging_root(data_dir, ai_memory_wiki::backup::running_in_container()),
    )
}

/// Where hook scripts are staged — which is also the path written into
/// the agent's settings, so it must be reachable by the process that
/// EXECUTES the hooks: the host-side agent CLI.
///
/// - **Native installs**: the resolved data dir (`--data-dir` /
///   `AI_MEMORY_DATA_DIR` / platform default), per #554.
/// - **Inside a container** (the docker wrapper): the server data dir is
///   a volume (`/data`) the host cannot execute from — staging there
///   wrote container-only paths into the host's settings and every hook
///   failed silently (#581). The wrapper binds `$HOME:$HOME` and reads
///   staged hooks from `~/.local/share/ai-memory/hooks` (its
///   `HOOKS_STAGE_DIR` contract), so the home-based default is the
///   host-reachable location there.
pub(crate) fn hook_staging_root(data_dir: &Path, in_container: bool) -> PathBuf {
    if in_container {
        dirs::data_local_dir()
            .map(|d| d.join("ai-memory"))
            .unwrap_or_else(|| data_dir.to_path_buf())
    } else {
        data_dir.to_path_buf()
    }
}

fn stage_hook_scripts_in(source_dir: &Path, agent_label: &str, data_dir: &Path) -> Result<PathBuf> {
    // `<data_dir>/hooks/<agent>`, not `<platform default>/ai-memory/hooks/…`.
    // Byte-identical for a default install, since `default_data_dir()` *is*
    // `data_local_dir()/ai-memory` — but it now follows `--data-dir` and
    // `AI_MEMORY_DATA_DIR` instead of populating a directory the operator is
    // not using (#554).
    let dest_root = data_dir.join("hooks").join(agent_label);

    fs::create_dir_all(&dest_root)
        .with_context(|| format!("creating staging dir {}", dest_root.display()))?;

    // When `resolve_hooks_dir` falls through to the data-local
    // candidate (e.g. docker `setup-agent` already extracted the
    // bundle into ~/.local/share/ai-memory/hooks/<agent>/, or a prior
    // install left scripts in place), the source dir IS the
    // destination dir. The wipe-then-copy flow below would delete the
    // very scripts we mean to install before reading them, leaving 0
    // copied and a settings.json pointing at an empty directory
    // (issue #52). Detect that case via canonical paths and verify
    // the existing layout in place instead of touching it.
    let same_path = same_canonical_dir(source_dir, &dest_root);

    if !same_path {
        // Wipe any previously-staged scripts that the current bundle
        // no longer ships. Idempotent re-runs against an old install
        // shouldn't leave stale entries pointed at by nothing.
        if let Ok(entries) = fs::read_dir(&dest_root) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() && is_hook_script_file(&p) {
                    fs::remove_file(&p).ok();
                }
            }
        }
    }

    let mut count = 0_usize;
    for entry in fs::read_dir(source_dir)
        .with_context(|| format!("reading source bundle {}", source_dir.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        if !from.is_file() || !is_hook_script_file(&from) {
            continue;
        }
        if !same_path {
            copy_hook_file(&from, &dest_root)?;
        }
        count += 1;
    }

    if !same_path {
        copy_support_hook_scripts(source_dir, &dest_root)?;

        // Stage the shared `_lib.sh` helper alongside the event scripts so
        // they can `. "$(dirname "$0")/_lib.sh"` without depending on the
        // user's PATH or repo layout. The helper lives ONCE in
        // `hooks/_lib.sh` (one parent up from the agent-specific dir) —
        // staging it here is what keeps every agent's runtime view
        // consistent with the source of truth.
        if let Some(shared) = source_dir.parent().map(|p| p.join("_lib.sh"))
            && shared.is_file()
        {
            copy_hook_file(&shared, &dest_root)?;
        }
    }

    if count == 0 {
        anyhow::bail!(
            "no hook scripts found at {}.\n\
             Refusing to install — pointing the agent's settings at an empty \
             directory would silently disable all capture. Either pass \
             `--hooks-dir <path>` to point at a populated source tree, or run \
             `ai-memory setup-agent --agent <name>` first to extract the \
             bundled scripts.",
            source_dir.display()
        );
    }

    let verb = if same_path { "verified" } else { "staged" };
    eprintln!("✓ {verb} {count} hook script(s) → {}", dest_root.display());
    Ok(dest_root)
}

/// `true` when `a` and `b` resolve to the same directory after symlink
/// canonicalization. Falls back to literal `==` if either canonicalize
/// call fails (e.g. dest hasn't been created yet on Windows, network
/// FS quirks). The caller has already `create_dir_all`'d both ends
/// in the staging flow, so the fast path almost always wins.
fn same_canonical_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// Copy a single hook file (event script or shared `_lib.sh`) into the
/// staging dir, preserving the executable bit on Unix. Centralised so
/// the script bulk-copy and the `_lib.sh` companion follow the same
/// rules without duplicating permission-handling.
fn copy_hook_file(from: &Path, dest_root: &Path) -> Result<()> {
    let to = dest_root.join(from.file_name().context("bad source file name")?);
    fs::copy(from, &to)
        .with_context(|| format!("copying {} → {}", from.display(), to.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&to)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&to, perms)?;
    }
    Ok(())
}

/// Copy the optional `lib/` support directory (currently PowerShell
/// helpers for Windows hook parity) alongside the event scripts.
/// No-op when the source bundle doesn't ship it.
fn copy_support_hook_scripts(source_dir: &Path, dest_root: &Path) -> Result<()> {
    let Some(source_hooks_root) = source_dir.parent() else {
        return Ok(());
    };
    let source_lib = source_hooks_root.join("lib");
    if !source_lib.is_dir() {
        return Ok(());
    }
    let Some(dest_hooks_root) = dest_root.parent() else {
        return Ok(());
    };
    let dest_lib = dest_hooks_root.join("lib");
    fs::create_dir_all(&dest_lib)
        .with_context(|| format!("creating hook support dir {}", dest_lib.display()))?;
    for entry in fs::read_dir(&source_lib)
        .with_context(|| format!("reading hook support dir {}", source_lib.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        if !from.is_file() || from.extension().and_then(|s| s.to_str()) != Some("ps1") {
            continue;
        }
        let to = dest_lib.join(from.file_name().context("bad support file name")?);
        fs::copy(&from, &to)
            .with_context(|| format!("copying {} → {}", from.display(), to.display()))?;
    }
    Ok(())
}

fn staged_command_dir(staged: &Path, agent_label: &str) -> PathBuf {
    match std::env::var("AI_MEMORY_HOOKS_HOST_ROOT") {
        Ok(root) if !root.trim().is_empty() => PathBuf::from(root).join(agent_label),
        _ => staged.to_path_buf(),
    }
}

fn is_hook_script_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("sh" | "ps1")
    )
}

fn resolve_hooks_dir(
    explicit: Option<&Path>,
    agent: AgentChoice,
    data_dir: &Path,
) -> Result<PathBuf> {
    let Some(sub) = agent.script_hook_subdir() else {
        anyhow::bail!("{agent:?} uses a generated integration, not a hook script directory")
    };
    if let Some(p) = explicit {
        let path = p.join(sub);
        if path.is_dir() {
            return Ok(path);
        }
        anyhow::bail!("hooks directory {} does not exist", path.display());
    }

    // Probe candidates in order. The first dir that exists wins.
    let candidates = hook_source_candidates(
        sub,
        repo_root_guess(),
        exe_dir_guess(),
        Some(data_dir.to_path_buf()),
    );
    for path in &candidates {
        if !path.as_os_str().is_empty() && path.is_dir() {
            return Ok(path.clone());
        }
    }
    anyhow::bail!(
        "could not locate hooks directory. Tried: {candidates:?}. \
         Pass --hooks-dir <directory containing {sub}/> to point at the bundle explicitly.",
    );
}

fn hook_source_candidates(
    sub: &str,
    repo_root: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(5);
    // Cargo-run from the repo.
    if let Some(root) = repo_root {
        candidates.push(root.join("hooks").join(sub));
    }
    // Release tarball (macOS/Windows/Linux archive): the `hooks/` bundle
    // ships in the same directory as the binary, so it's reachable without
    // `--source` (issue #107).
    if let Some(dir) = exe_dir {
        candidates.push(dir.join("hooks").join(sub));
    }
    // Docker image lays them out under /usr/local/share/ai-memory/.
    candidates.push(PathBuf::from(format!(
        "/usr/local/share/ai-memory/hooks/{sub}"
    )));
    // Native Linux packages install hook sources under /usr/share.
    candidates.push(PathBuf::from(format!("/usr/share/ai-memory/hooks/{sub}")));
    // Local install honourable mention: the bundle a previous `--apply`, or
    // docker `setup-agent`, staged under the data dir actually in use.
    if let Some(dir) = data_dir {
        candidates.push(dir.join("hooks").join(sub));
    }
    candidates
}

fn repo_root_guess() -> Option<PathBuf> {
    repo_root_from_exe(&current_exe_resolved()?)
}

/// When the binary lives under target/{debug,release}/<name>, the
/// workspace root is two parents up.
fn repo_root_from_exe(exe: &Path) -> Option<PathBuf> {
    Some(exe.parent()?.parent()?.parent()?.to_path_buf())
}

/// Directory the running binary lives in. The release tarball ships the
/// `hooks/` bundle right next to the binary, so a no-`--source`
/// `install-hooks` finds it there (issue #107).
fn exe_dir_guess() -> Option<PathBuf> {
    Some(current_exe_resolved()?.parent()?.to_path_buf())
}

/// Path of the running binary with symlinks resolved.
///
/// Both guesses above are relative to where the binary *lives*, not to how
/// it was invoked. `~/.local/bin/ai-memory -> <repo>/target/release/ai-memory`
/// is the layout the quick start suggests, and without this the repo-root
/// guess walks up from the link's own directory to `$HOME` while the tarball
/// guess stops at `~/.local/bin` — leaving the bundled `hooks/` unreachable
/// from every candidate, whatever the cwd (issue #546).
fn current_exe_resolved() -> Option<PathBuf> {
    std::env::current_exe().ok().map(resolve_exe_path)
}

/// Canonicalise an executable path, keeping the original when the target
/// cannot be resolved (deleted binary, or a filesystem that refuses).
fn resolve_exe_path(exe: PathBuf) -> PathBuf {
    fs::canonicalize(&exe).unwrap_or(exe)
}

// CLAUDE_CODE_EVENTS + build_claude_code_payload now live in
// `super::render_shared`, shared with `setup-agent`.

/// Which optional capture surfaces the generated Claude Code settings should
/// include.
///
/// Grouped rather than passed as two positional bools: at the call site
/// `true, false` said nothing about which surface was which, and the pair is
/// exactly the privacy-relevant part of this install — worth naming.
#[derive(Debug, Clone, Copy)]
struct ClaudeCaptureScope {
    /// Include the assistant-message capture hook (double opt-in, off by
    /// default).
    assistant: bool,
    /// Include the `UserPromptSubmit` hook. When false the entry is omitted
    /// entirely, so the agent never emits prompt text at all.
    prompts: bool,
}

fn render_claude_code(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    settings_path: &Path,
    capture: ClaudeCaptureScope,
) -> Result<()> {
    let ClaudeCaptureScope {
        assistant: capture_assistant,
        prompts: capture_prompts,
    } = capture;
    // Soft check: warn (don't bail) if a script is missing. The user
    // may be running this command inside docker against a host path
    // that exists only on the host's filesystem — bailing would
    // sabotage the docker-only flow `setup-agent` enables.
    for (event, script) in super::render_shared::CLAUDE_CODE_EVENTS {
        if !capture_prompts && event == CLAUDE_PROMPT_EVENT {
            continue;
        }
        let script = hook_script_for_claude_code(script);
        let abs = hooks_dir.join(script.as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. \
                 If this command is running inside docker against a \
                 host path, you can ignore this; otherwise extract \
                 the scripts first with `ai-memory setup-agent`.",
                abs.display()
            );
        }
    }
    let payload = configure_claude_prompt_capture(
        build_claude_code_payload_with_data_dir(
            hooks_dir,
            server_url,
            auth_token,
            Some(data_dir),
            project_strategy,
            capture_assistant,
        ),
        capture_prompts,
    );
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing claude code hook config")?;
    println!(
        "# Claude Code hook config — merge into {}",
        settings_path.display()
    );
    println!("# Hook scripts: {}", hooks_dir.display());
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook command below.");
        println!(
            "#       Treat {} as sensitive (chmod 600).",
            settings_path.display()
        );
    }
    println!();
    println!("{serialized}");
    Ok(())
}

/// Whether a hook entry in ZCode's `hooks.events` map is one ai-memory
/// wrote. Our entries carry a `statusMessage` starting with `ai-memory`
/// — the only schema-documented free-form key, so ownership is marked
/// without emitting keys ZCode would drop.
fn is_our_zcode_hook(hook: &serde_json::Value) -> bool {
    hook.get("statusMessage")
        .and_then(|v| v.as_str())
        .is_some_and(|msg| msg.starts_with("ai-memory"))
}

/// Withdraw ai-memory's own hook entries from every matcher group in
/// `groups`, leaving each group, and anyone else's hooks inside it, in
/// place.
fn strip_our_zcode_hook_entries(groups: &mut [serde_json::Value]) {
    for group in groups.iter_mut() {
        if let Some(hooks) = group
            .as_object_mut()
            .and_then(|group| group.get_mut("hooks"))
            .and_then(|v| v.as_array_mut())
        {
            hooks.retain(|hook| !is_our_zcode_hook(hook));
        }
    }
}

/// Same, for a `hooks.events` slot of unknown shape. Returns how many
/// entries were withdrawn so the caller can report them; a slot that is
/// not an array is left untouched and counts zero.
fn strip_our_zcode_hooks(slot: &mut serde_json::Value) -> usize {
    let Some(groups) = slot.as_array_mut() else {
        return 0;
    };
    let before = zcode_hook_count(groups);
    strip_our_zcode_hook_entries(groups);
    before.saturating_sub(zcode_hook_count(groups))
}

/// Total hook entries across every matcher group in `groups`, ignoring
/// groups whose `hooks` is absent or not an array.
fn zcode_hook_count(groups: &[serde_json::Value]) -> usize {
    groups
        .iter()
        .filter_map(|group| group.get("hooks").and_then(|v| v.as_array()))
        .map(|hooks| hooks.len())
        .sum()
}

/// True when a `hooks.events` entry cannot run anything: no matcher groups
/// at all, no group carrying a non-empty `hooks` array, or a value that is
/// not a matcher-group array in the first place. Such a key is pure
/// downside under ZCode's strict validation, since it runs nothing and can
/// cost the user every other hook in the block.
///
/// Deliberately narrower than "a key ai-memory does not write": several of
/// those are real ZCode events this tool skips on purpose (see
/// `PermissionRequest` in `render_shared`), and a key still running
/// someone's hook is their working config, not a leftover. Reporting on
/// that set would be a false-positive generator.
fn zcode_event_runs_nothing(slot: &serde_json::Value) -> bool {
    match slot.as_array() {
        Some(groups) => zcode_hook_count(groups) == 0,
        None => true,
    }
}

/// Merge ai-memory hooks into the root `hooks` block of ZCode's
/// user-scope `~/.zcode/cli/config.json` (#512). Sibling config keys
/// (`model`, `provider`, `mcp.servers`, …) always survive: the merge
/// touches only `hooks.enabled` (defaulted, never flipped),
/// `hooks.maxOutputBytes` (set only when absent), and `hooks.events`,
/// replaces entries ai-memory owns (idempotent re-apply), and never
/// removes a hook it did not write. Entries it owns are withdrawn
/// wherever they sit, including under event keys this version no longer
/// writes; event keys themselves are never deleted, only reported when
/// ai-memory withdrew entries from them, or when they are left unable to
/// run anything (#600). The one thing it does
/// discard is a matcher group left holding an empty `hooks` array under a
/// key it writes, which drops a caller's own empty group along with the
/// ones its withdrawal emptied.
fn apply_to_zcode_hooks(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => zcode_config_path()?,
    };
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let payload = super::render_shared::build_zcode_hooks_config(
        server_url,
        auth_token,
        Some(data_dir),
        strategy,
    );
    let our_events = payload
        .get("events")
        .and_then(|v| v.as_object())
        .context("internal: build_zcode_hooks_config didn't return an events map")?
        .clone();
    let mut hooks_disabled = false;
    let mut stale: Vec<(String, usize, bool)> = Vec::new();
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            // `hooks` sits beside sibling config keys that must survive.
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .context("`hooks` is present in the ZCode config but not an object")?;
            hooks_disabled = hooks.get("enabled").and_then(|v| v.as_bool()) == Some(false);
            if !hooks.contains_key("enabled") {
                hooks.insert("enabled".into(), serde_json::Value::Bool(true));
            }
            // Raise the stdout ceiling above ZCode's 32 KiB default so a
            // fetched handoff is not truncated mid-JSON before injection.
            // Only when the user has not chosen their own value: the key
            // is block-level and would also bound third-party hooks.
            if !hooks.contains_key("maxOutputBytes")
                && let Some(ceiling) = payload.get("maxOutputBytes")
            {
                hooks.insert("maxOutputBytes".into(), ceiling.clone());
            }
            let events = hooks
                .entry("events")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .context("`hooks.events` is present in the ZCode config but not an object")?;
            for (event, ours) in &our_events {
                let ours = ours
                    .as_array()
                    .context("internal: zcode event payload is not a matcher-group array")?;
                let slot = events
                    .entry(event.clone())
                    .or_insert_with(|| serde_json::Value::Array(Vec::new()));
                let slot = slot.as_array_mut().context(
                    "`hooks.events.{event}` is present in the ZCode config but not an array",
                )?;
                strip_our_zcode_hook_entries(slot);
                slot.retain(|group| match group.get("hooks") {
                    Some(serde_json::Value::Array(hooks)) => !hooks.is_empty(),
                    _ => true,
                });
                slot.extend(ours.iter().cloned());
            }
            // ai-memory writes only the six keys in `ZCODE_EVENTS`, but its
            // own entries can sit under other keys too (a config carried
            // over from another agent, or hand-edited). Those are ours to
            // withdraw; the keys themselves are not, so every one is left
            // in place. What makes a leftover dangerous is ZCode strict-
            // validating this map: one key it does not recognize and it
            // rejects the ENTIRE hooks block, ai-memory's included, so
            // capture dies while the file still matches byte for byte and
            // apply reports it as up to date (#600). Collect the keys that
            // can no longer run anything and say so after the write.
            for (event, slot) in events.iter_mut() {
                if our_events.contains_key(event) {
                    continue;
                }
                let withdrawn = strip_our_zcode_hooks(slot);
                let runs_nothing = zcode_event_runs_nothing(slot);
                if withdrawn > 0 || runs_nothing {
                    stale.push((event.clone(), withdrawn, runs_nothing));
                }
            }
            Ok(())
        })
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            // Only claim the install is current when nothing turned up
            // that can stop ZCode loading it. Saying "already up to date"
            // over a block ZCode rejects is the whole of #600, and the
            // notes explaining it go to stderr, so a redirected stdout
            // would otherwise carry the reassurance and none of the cause.
            ApplyOutcome::NoOp if stale.is_empty() => "already up to date",
            ApplyOutcome::NoOp => "unchanged; see the notes below",
        }
    );
    if hooks_disabled {
        eprintln!(
            "# warning: this config sets \"enabled\": false inside `hooks`, \
             so ZCode will not run ANY hooks (including ai-memory's) until \
             you re-enable them."
        );
    }
    if !stale.is_empty() {
        // The report above goes to stdout and these go to stderr; stdout
        // block-buffers when redirected, so without this the two arrive out
        // of order in a log or in CI.
        let _ = std::io::stdout().flush();
    }
    for (event, withdrawn, runs_nothing) in &stale {
        if *withdrawn > 0 {
            // A withdrawal is positive evidence ai-memory once lived under
            // this key, so the risk here is not hypothetical.
            eprintln!(
                "# warning: withdrew {withdrawn} ai-memory hook(s) from \
                 `hooks.events.{event}`, a key ai-memory does not write. The \
                 key itself was left as found, but ZCode validates this map \
                 strictly and rejects the ENTIRE hooks block, ai-memory's \
                 included, over one key it does not recognize, which leaves \
                 capture silently dead. If ZCode reports `hookCount: 0`, \
                 remove that key from {}.",
                path.display()
            );
        } else if *runs_nothing {
            eprintln!(
                // Only the provenance claim degrades between the two
                // tiers; the consequence is identical, so state it at full
                // strength. "If ZCode does not recognize it" keeps this
                // honest about keys ZCode does accept, such as an empty
                // `Notification`.
                "# note: `hooks.events.{event}` runs nothing, and ai-memory \
                 does not write that key. If ZCode does not recognize it, \
                 ZCode rejects the ENTIRE hooks block, ai-memory's included, \
                 and capture is dead while this file looks correct. If ZCode \
                 reports `hookCount: 0`, remove that key from {}.",
                path.display()
            );
        }
    }
    println!("# NOTE: ZCode injects SessionStart stdout as model context, so the");
    println!("#       prior session's handoff is delivered automatically.");
    println!("# NOTE: ZCode fires `Stop` at the end of every turn and has no");
    println!("#       SessionEnd — close sessions with");
    println!("#       `ai-memory finalize-session --agent zcode`.");
    Ok(())
}

/// Print ZCode's `hooks` block to stdout (dry-run counterpart of
/// [`apply_to_zcode_hooks`]).
fn render_zcode(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    let payload = super::render_shared::build_zcode_hooks_config(
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing zcode hook config")?;
    println!("# ZCode (z.ai) hooks config — merge the `hooks` block into");
    println!("# ~/.zcode/cli/config.json (the same file `install-mcp --client");
    println!("# zcode` registers MCP servers in), or re-run with --apply to");
    println!("# merge it in place, preserving the rest of the config.");
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: this printed snippet embeds the token in each hook's args");
        println!("#       so a hand-placed config works as-is — treat it as");
        println!("#       sensitive (chmod 600). NOTE: `--apply` does NOT embed the");
        println!("#       token; it persists it to <data-dir>/auth-token (0600) and");
        println!("#       writes token-less args, so the applied config differs from");
        println!("#       this snippet by design (#552, #600).");
    }
    println!("# NOTE: ZCode injects SessionStart stdout as model context, so the");
    println!("#       prior session's handoff is delivered automatically.");
    println!("# NOTE: ZCode fires `Stop` at the end of every turn and has no");
    println!("#       SessionEnd — close sessions with");
    println!("#       `ai-memory finalize-session --agent zcode`.");
    println!();
    println!("{serialized}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The marker-presence admit gate the shared `ts_capture_policy_v1`
    /// template bakes into `capturePolicy` (#661). Under allowlist a
    /// repository with no `.ai-memory.toml` marker must emit nothing —
    /// mirroring `repository_admits_capture` in
    /// `ai-memory-hooks::capture_policy`, the gate hook.rs's native path
    /// runs before any per-event disposition. Keyed on `findMarker(cwd)`
    /// presence rather than `config.state`, so a marker with an empty
    /// `[capture]` section — `state` is "inactive" either way — still admits
    /// capture.
    const CAPTURE_ADMIT_GATE_TS: &str = "const markerPresent = !!findMarker(cwd); if (CAPTURE_MODE === \"allowlist\" && !markerPresent) return { disposition: \"drop\", payload };";

    /// #446's binding requirement: "a protection that disappears on upgrade
    /// without saying so is worse than no protection". A bare `--apply` — what
    /// `ai-memory upgrade` runs — must leave an existing opt-in alone.
    #[test]
    fn bare_apply_preserves_an_existing_allowlist_optin() {
        let tmp = tempfile::tempdir().unwrap();
        let mode = persist_capture_mode(tmp.path(), Some(CaptureModeArg::Allowlist)).unwrap();
        assert_eq!(mode, "allowlist");

        // The upgrade path: no flag at all.
        let after = persist_capture_mode(tmp.path(), None).unwrap();
        assert_eq!(
            after, "allowlist",
            "a bare re-apply must not revert the opt-in"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join(crate::commands::hook::CAPTURE_MODE_FILE))
                .unwrap()
                .trim(),
            "allowlist"
        );
    }

    /// #446's opt-in is stored as a bare word, and every reader maps anything
    /// it does not recognise onto `denylist` — the *less* private mode. That
    /// fallback is deliberate and tested
    /// (`hook.rs`: `unreadable_or_unknown_mode_falls_back_to_denylist_not_allowlist`);
    /// what must never happen is the truncated file that triggers it. An
    /// in-place write truncates before it writes, so a crash, a full disk or a
    /// killed `upgrade` mid-write leaves exactly that — and the next hook run
    /// reads it as capture-by-default, silently undoing the opt-in the doc
    /// comment on `persist_capture_mode` promises to preserve.
    ///
    /// Replacement rather than truncation is the property that rules the window
    /// out, and it is observable: a rename gives the path a new inode, an
    /// in-place rewrite keeps the old one. This fails against `fs::write`.
    #[cfg(unix)]
    #[test]
    fn rewriting_the_capture_mode_replaces_the_file_rather_than_truncating_it() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(crate::commands::hook::CAPTURE_MODE_FILE);

        persist_capture_mode(tmp.path(), Some(CaptureModeArg::Allowlist)).unwrap();
        let before = fs::metadata(&path).unwrap().ino();

        persist_capture_mode(tmp.path(), Some(CaptureModeArg::Denylist)).unwrap();
        let after = fs::metadata(&path).unwrap().ino();

        assert_ne!(
            before, after,
            "the capture mode must be replaced by rename, not rewritten in \
             place: an in-place write is observable truncated, and a truncated \
             file reads back as denylist"
        );

        let leftovers: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".ai-memory-tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "the staging tempfile must not survive the write: {leftovers:?}"
        );
    }

    #[test]
    fn absent_file_reports_the_historical_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(persist_capture_mode(tmp.path(), None).unwrap(), "denylist");
    }

    #[test]
    fn explicit_denylist_downgrades_an_earlier_optin() {
        // The opt-in must be reversible, or operators cannot undo a mistake.
        let tmp = tempfile::tempdir().unwrap();
        persist_capture_mode(tmp.path(), Some(CaptureModeArg::Allowlist)).unwrap();
        assert_eq!(
            persist_capture_mode(tmp.path(), Some(CaptureModeArg::Denylist)).unwrap(),
            "denylist"
        );
        assert_eq!(persist_capture_mode(tmp.path(), None).unwrap(), "denylist");
    }
    use crate::cli::ProjectStrategyArg;
    use std::collections::BTreeMap;
    use std::fs;
    #[cfg(any(unix, windows))]
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn capture_assistant_allowed_only_for_claude_native() {
        use crate::cli::AgentChoice::*;
        // Every non-Claude agent is rejected regardless of platform (#196): the
        // opt-in cannot take effect for them, so the installer must bail.
        for agent in [OpenCode, OpenCode2, Zcode] {
            assert!(
                !capture_assistant_allowed(agent),
                "{agent:?} must not allow --capture-assistant"
            );
        }
        // Claude Code tracks the native-platform gate exactly.
        assert_eq!(
            capture_assistant_allowed(ClaudeCode),
            local_hook_policy_v1_supported()
        );
    }

    #[test]
    fn prompt_capture_options_are_claude_code_only() {
        use crate::cli::AgentChoice::*;
        assert!(prompt_capture_options_allowed(ClaudeCode));
        for agent in [OpenCode, OpenCode2, Zcode] {
            assert!(!prompt_capture_options_allowed(agent), "{agent:?}");
        }
    }

    #[test]
    fn baked_prompt_capture_reads_only_owned_claude_hooks() {
        let enabled = serde_json::json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{ "command": "ai-memory hook --event session-start --agent claude-code --server-url http://h" }] }],
                "UserPromptSubmit": [{ "hooks": [{ "command": "ai-memory hook --event user-prompt --agent claude-code --server-url http://h" }] }]
            }
        });
        assert_eq!(
            baked_claude_prompt_capture(&enabled.to_string()),
            Some(true)
        );

        let disabled = serde_json::json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{ "command": "ai-memory hook --event session-start --agent claude-code --server-url http://h" }] }],
                "UserPromptSubmit": [{ "hooks": [{ "command": "third-party prompt guard" }] }]
            }
        });
        assert_eq!(
            baked_claude_prompt_capture(&disabled.to_string()),
            Some(false)
        );

        let unrelated = serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [{ "hooks": [{ "command": "third-party prompt guard" }] }]
            }
        });
        assert_eq!(baked_claude_prompt_capture(&unrelated.to_string()), None);
    }

    #[cfg(unix)]
    fn bash_program_for_installer_test() -> Option<std::path::PathBuf> {
        Some(std::path::PathBuf::from("bash"))
    }

    #[cfg(windows)]
    fn bash_program_for_installer_test() -> Option<std::path::PathBuf> {
        let mut candidates = Vec::new();
        if let Some(root) = std::env::var_os("EXEPATH") {
            let root = std::path::PathBuf::from(root);
            candidates.push(root.join("bin").join("bash.exe"));
            candidates.push(root.join("usr").join("bin").join("bash.exe"));
        }
        for env_key in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
            if let Some(root) = std::env::var_os(env_key) {
                let root = std::path::PathBuf::from(root).join("Git");
                candidates.push(root.join("bin").join("bash.exe"));
                candidates.push(root.join("usr").join("bin").join("bash.exe"));
            }
        }
        candidates.sort();
        candidates.dedup();
        let found = candidates.into_iter().find(|candidate| candidate.is_file());
        if found.is_none() {
            eprintln!("skipping installer shell contract: Git for Windows bash.exe was not found");
        }
        found
    }

    #[test]
    fn overlay_event_hooks_preserves_third_party_and_replaces_own() {
        // Regression for issue #80: install-hooks must MERGE into the event
        // array, not replace it. A third-party SessionStart hook (e.g.
        // context-mode) must survive while our own stale entry is swapped
        // for the fresh one.
        let mut hooks = serde_json::Map::new();
        hooks.insert(
            "SessionStart".into(),
            serde_json::json!([
                { "hooks": [ { "type": "command", "command": "node context-mode-cache-heal.mjs" } ] },
                { "matcher": "", "hooks": [ { "type": "command", "command": "/old/ai-memory.exe hook --event session-start --agent claude-code --server-url http://old" } ] }
            ]),
        );
        let ours = serde_json::json!([
            { "matcher": "", "hooks": [ { "type": "command", "command": "/new/.cargo/bin/ai-memory.exe hook --event session-start --agent claude-code --server-url http://new" } ] }
        ]);
        overlay_event_hooks(&mut hooks, "SessionStart", &ours);

        let arr = hooks["SessionStart"].as_array().unwrap();
        assert_eq!(
            arr.len(),
            2,
            "third-party + our single fresh entry: {arr:?}"
        );
        let joined = serde_json::to_string(arr).unwrap();
        assert!(
            joined.contains("context-mode-cache-heal"),
            "third-party hook must survive"
        );
        assert!(
            !joined.contains("/old/ai-memory.exe"),
            "stale ai-memory entry must be replaced"
        );
        assert!(
            joined.contains("/new/.cargo/bin/ai-memory.exe"),
            "fresh ai-memory entry must be present"
        );
    }

    #[test]
    fn overlay_event_hooks_inserts_when_event_absent() {
        let mut hooks = serde_json::Map::new();
        let ours = serde_json::json!([
            { "matcher": "", "hooks": [ { "type": "command", "command": "ai-memory.exe hook --event stop --agent claude-code --server-url http://h" } ] }
        ]);
        overlay_event_hooks(&mut hooks, "Stop", &ours);
        assert_eq!(hooks["Stop"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn overlay_event_hooks_is_idempotent_on_reapply() {
        // Re-applying must not accumulate duplicate ai-memory entries.
        let mut hooks = serde_json::Map::new();
        let ours = serde_json::json!([
            { "matcher": "", "hooks": [ { "type": "command", "command": "ai-memory.exe hook --event pre-tool-use --agent claude-code --server-url http://h" } ] }
        ]);
        overlay_event_hooks(&mut hooks, "PreToolUse", &ours);
        overlay_event_hooks(&mut hooks, "PreToolUse", &ours);
        assert_eq!(
            hooks["PreToolUse"].as_array().unwrap().len(),
            1,
            "no duplicates on re-apply"
        );
    }

    #[test]
    fn is_ai_memory_hook_entry_detects_nested_flat_and_skips_third_party() {
        // Nested (Claude Code / Codex / Gemini)
        assert!(is_ai_memory_hook_entry(&serde_json::json!(
            { "matcher": "", "hooks": [ { "type": "command", "command": "ai-memory.exe hook --event session-start --agent claude-code --server-url http://h" } ] }
        )));
        // Flat (Cursor) + shell form
        assert!(is_ai_memory_hook_entry(&serde_json::json!(
            { "type": "command", "command": "bash -c 'AI_MEMORY_HOOK_URL=x /c/x/ai-memory/hooks/pre.sh'" }
        )));
        // Claude Code exec form
        assert!(is_ai_memory_hook_entry(&serde_json::json!(
            { "matcher": "", "hooks": [ { "type": "command", "command": "C:\\bin\\ai-memory.exe", "args": ["hook", "--event", "session-start", "--agent", "claude-code", "--server-url", "http://h"] } ] }
        )));
        // Third-party must NOT be flagged
        assert!(!is_ai_memory_hook_entry(&serde_json::json!(
            { "hooks": [ { "type": "command", "command": "node context-mode-cache-heal.mjs" } ] }
        )));
        assert!(!is_ai_memory_hook_entry(&serde_json::json!(
            { "hooks": [ { "type": "command", "command": "C:\\bin\\third-party.exe", "args": ["hook", "--event", "session-start", "--agent", "claude-code", "--server-url", "http://h"] } ] }
        )));
        assert!(!is_ai_memory_hook_entry(&serde_json::json!(
            { "hooks": [ { "type": "command", "command": "C:\\bin\\ai-memory-helper.exe", "args": ["--check", "project"] } ] }
        )));
        // A third-party string command may use the same executable name, but
        // without either ai-memory hook signature it must not be replaced.
        assert!(!is_ai_memory_hook_entry(&serde_json::json!(
            { "hooks": [ { "type": "command", "command": "/opt/alphaone/bin/ai-memory boot --quiet --limit 10 --budget-tokens 4096" } ] }
        )));
        // Legacy script commands can use any path, so the exact env marker is
        // the ownership signal rather than the script basename.
        assert!(is_ai_memory_hook_entry(&serde_json::json!(
            { "hooks": [ { "type": "command", "command": "AI_MEMORY_HOOK_URL=http://h /custom/session-start.sh" } ] }
        )));
    }

    #[test]
    fn overlay_event_hooks_preserves_third_party_string_command_and_replaces_legacy() {
        let third_party =
            "/opt/alphaone/bin/ai-memory boot --quiet --limit 10 --budget-tokens 4096";
        let legacy = "AI_MEMORY_HOOK_URL=http://old /custom/session-start.sh";
        let mut hooks = serde_json::Map::new();
        hooks.insert(
            "SessionStart".into(),
            serde_json::json!([
                { "matcher": "", "hooks": [ { "type": "command", "command": third_party } ] },
                { "matcher": "", "hooks": [ { "type": "command", "command": legacy } ] }
            ]),
        );
        let ours = serde_json::json!([
            { "matcher": "", "hooks": [ { "type": "command", "command": "AI_MEMORY_HOOK_URL=http://new /fresh/session-start.sh" } ] }
        ]);

        overlay_event_hooks(&mut hooks, "SessionStart", &ours);

        let commands: Vec<&str> = hooks["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| {
                entry
                    .pointer("/hooks/0/command")
                    .and_then(serde_json::Value::as_str)
            })
            .collect();
        assert_eq!(
            commands,
            [
                third_party,
                "AI_MEMORY_HOOK_URL=http://new /fresh/session-start.sh"
            ]
        );
        assert!(!commands.contains(&legacy), "legacy entry must be replaced");
    }

    fn stub_scripts(dir: &Path, names: &[&str]) {
        for name in names {
            let p = dir.join(name);
            fs::write(&p, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&p).unwrap().permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&p, perms).unwrap();
            }
        }
    }

    fn default_hook_args() -> InstallHooksArgs {
        InstallHooksArgs {
            profile: None,
            agent: AgentChoice::OpenCode,
            capture_assistant: false,
            no_capture_prompts: false,
            capture_mode: None,
            capture_prompts: false,
            hooks_dir: None,
            server_url: None,
            auth_token: None,
            as_user: None,
            apply: true,
            config_file: None,
            project_strategy: Some(ProjectStrategyArg::Basename),
        }
    }

    // ── #issue project-strategy preservation on re-apply ─────────────

    #[test]
    fn project_strategy_from_text_reads_every_form() {
        // Shell env prefix (Claude Code / POSIX script hooks).
        assert_eq!(
            project_strategy_from_text(
                "AI_MEMORY_HOOK_URL=http://h AI_MEMORY_PROJECT_STRATEGY=repo-root /x/s.sh"
            ),
            Some(ProjectStrategyArg::RepoRoot)
        );
        // PowerShell form.
        assert_eq!(
            project_strategy_from_text("$env:AI_MEMORY_PROJECT_STRATEGY='repo-root'; & /x/s.ps1"),
            Some(ProjectStrategyArg::RepoRoot)
        );
        // Native flag, both spellings.
        assert_eq!(
            project_strategy_from_text("ai-memory hook --project-strategy repo-root run"),
            Some(ProjectStrategyArg::RepoRoot)
        );
        assert_eq!(
            project_strategy_from_text("ai-memory hook --project-strategy=repo_root"),
            Some(ProjectStrategyArg::RepoRoot)
        );
        assert_eq!(
            project_strategy_from_text("const DEFAULT_PROJECT_STRATEGY = \"repo-root\";"),
            Some(ProjectStrategyArg::RepoRoot)
        );
    }

    #[test]
    fn baked_project_strategy_ignores_unowned_json_entry() {
        let existing = serde_json::json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{
                    "command": "third-party",
                    "args": ["--project-strategy", "repo-root"]
                }]}]
            }
        })
        .to_string();
        assert_eq!(
            baked_project_strategy(AgentChoice::ClaudeCode, &existing),
            None
        );
    }

    fn install_project_strategy_preserves_baked_when_flag_absent() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("settings.json");
        std::fs::write(
            &cfg,
            serde_json::json!({
                "hooks": { "SessionStart": [{ "hooks": [{
                    "command": "AI_MEMORY_HOOK_URL=http://h AI_MEMORY_PROJECT_STRATEGY=repo-root /x/ai-memory/session-start.sh"
                }]}] }
            })
            .to_string(),
        )
        .unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::ClaudeCode,
            config_file: Some(cfg),
            project_strategy: None,
            ..default_hook_args()
        };
        // A bare re-apply (e.g. `ai-memory upgrade`) must keep repo-root.
        assert_eq!(
            install_project_strategy(&args),
            Some(ProjectStrategyArg::RepoRoot)
        );
    }

    #[test]
    fn install_project_strategy_explicit_basename_overrides_existing() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("settings.json");
        std::fs::write(
            &cfg,
            serde_json::json!({
                "hooks": { "SessionStart": [{ "hooks": [{
                    "command": "AI_MEMORY_HOOK_URL=http://h AI_MEMORY_PROJECT_STRATEGY=repo-root /x/ai-memory/session-start.sh"
                }]}] }
            })
            .to_string(),
        )
        .unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::ClaudeCode,
            config_file: Some(cfg),
            project_strategy: Some(ProjectStrategyArg::Basename),
            ..default_hook_args()
        };
        // Explicit basename is honored, not overridden by the baked repo-root.
        assert_eq!(
            install_project_strategy(&args),
            Some(ProjectStrategyArg::Basename)
        );
    }

    #[test]
    fn install_project_strategy_none_when_target_absent() {
        let tmp = TempDir::new().unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::ClaudeCode,
            config_file: Some(tmp.path().join("nope.json")),
            project_strategy: None,
            ..default_hook_args()
        };
        assert_eq!(install_project_strategy(&args), None);
    }

    #[test]
    fn validate_as_user_passes_when_not_set() {
        assert!(validate_as_user(None, None).is_ok());
        assert!(validate_as_user(None, Some("tok")).is_ok());
    }

    /// Empty / whitespace-only `--as-user` is treated as not-set.
    /// Defensive: an accidental `--as-user ""` shouldn't bail.
    #[test]
    fn validate_as_user_treats_blank_as_unset() {
        assert!(validate_as_user(Some(""), None).is_ok());
        assert!(validate_as_user(Some("   "), None).is_ok());
    }

    /// `--as-user` with no `--auth-token` is the error case the v0.8
    /// docs warn about — without a token the hook scripts authenticate
    /// anonymously / as root, making the `--as-user X` label misleading.
    #[test]
    fn validate_as_user_bails_without_auth_token() {
        let err = validate_as_user(Some("alice"), None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--as-user 'alice'") && msg.contains("--auth-token"),
            "error must name both flags: {msg}"
        );
        // Empty auth token is treated the same as missing.
        assert!(validate_as_user(Some("alice"), Some("")).is_err());
        assert!(validate_as_user(Some("alice"), Some("   ")).is_err());
    }

    /// `--as-user X --auth-token <something>` passes — the install
    /// proceeds with X as metadata and the supplied token as the
    /// bearer.
    #[test]
    fn validate_as_user_passes_with_both_flags() {
        assert!(validate_as_user(Some("alice"), Some("some-token")).is_ok());
    }

    #[test]
    fn hook_server_url_defaults_to_configured_server_url() {
        let config = Config {
            server_url: "http://192.168.0.90:49374/".into(),
            ..Config::default()
        };
        let args = default_hook_args();

        assert_eq!(
            effective_hook_server_url(&config, &args, None),
            "http://192.168.0.90:49374"
        );
    }

    #[test]
    fn hook_server_url_explicit_flag_wins_over_config() {
        let config = Config {
            server_url: "http://homelab:49374".into(),
            ..Config::default()
        };
        let mut args = default_hook_args();
        args.server_url = Some("http://explicit:49374/".into());

        assert_eq!(
            effective_hook_server_url(&config, &args, None),
            "http://explicit:49374"
        );
    }

    /// Regression (found 2026-07-12 during Devin real-acceptance A/B
    /// testing): an explicit `--server-url` that happens to equal the
    /// compiled-in `DEFAULT_SERVER_URL` must still win over a configured
    /// (env/config.toml) server_url pointing somewhere else. Before the
    /// `Option<String>` fix, `args.server_url` was a plain `String` with
    /// `default_value_t = DEFAULT_SERVER_URL`, so clap couldn't
    /// distinguish "operator explicitly typed the default value" from
    /// "operator passed nothing at all" — both produced the same string,
    /// so this exact case silently fell through to `AI_MEMORY_SERVER_URL`
    /// / config.toml instead of honouring the explicit flag.
    #[test]
    fn hook_server_url_explicit_flag_matching_compiled_default_still_wins() {
        let config = Config {
            server_url: "http://127.0.0.1:49375".into(),
            ..Config::default()
        };
        let mut args = default_hook_args();
        args.server_url = Some(DEFAULT_SERVER_URL.to_string());

        assert_eq!(
            effective_hook_server_url(&config, &args, None),
            DEFAULT_SERVER_URL,
            "an explicit --server-url matching the compiled default must not be \
             silently overridden by a differently-configured server_url"
        );
    }

    /// Post-audit P1 — the new `ai-memory hook` subcommand (#84) builds
    /// its request URL by hand, skipping `Config::load` for latency. PR
    /// #82 made thin-client commands respect `AI_MEMORY_BASE_PATH` via
    /// `ServerEndpoint::build_url`, but the hook subcommand doesn't go
    /// through there — so a deployment under `--base-path /wiki` with
    /// the base set via env (not the URL path) had `ai-memory status`
    /// working and `ai-memory hook` 404'ing. Fix: install-hooks bakes
    /// the prefix into the URL it embeds, so hook.rs uses what it's
    /// given and stays unchanged.
    #[test]
    fn hook_server_url_threads_base_path_when_url_has_no_path() {
        let config = Config {
            server_url: "http://homelab:49374".into(),
            base_path: "/wiki".into(),
            ..Config::default()
        };
        let args = default_hook_args();
        assert_eq!(
            effective_hook_server_url(&config, &args, None),
            "http://homelab:49374/wiki",
            "URL baked into the hook command must carry the base-path so \
             `ai-memory hook` POSTs to /wiki/hook (not /hook)"
        );
    }

    /// If the operator already put the prefix into the URL itself, do
    /// NOT append `base_path` on top — that would double the prefix to
    /// `/wiki/wiki`.
    #[test]
    fn hook_server_url_does_not_double_base_path_when_already_in_url() {
        let config = Config {
            server_url: "http://homelab:49374/wiki".into(),
            base_path: "/wiki".into(),
            ..Config::default()
        };
        let args = default_hook_args();
        assert_eq!(
            effective_hook_server_url(&config, &args, None),
            "http://homelab:49374/wiki"
        );
    }

    #[test]
    fn hook_server_url_falls_back_to_existing_mcp_entry() {
        let config = Config::default();
        let args = default_hook_args();
        let inferred = InferredMcpConfig {
            hook_server_url: Some("http://homelab:49374".into()),
            auth_token: Some("tok".into()),
        };

        assert_eq!(
            effective_hook_server_url(&config, &args, Some(&inferred)),
            "http://homelab:49374"
        );
    }

    #[test]
    fn zcode_hooks_config_covers_all_events_with_strict_entries() {
        let payload = super::super::render_shared::build_zcode_hooks_config(
            "http://127.0.0.1:49374",
            Some("tok-test"),
            Some(Path::new("/data")),
            Some("repo-root"),
        );
        assert_eq!(payload["enabled"], serde_json::json!(true));
        // Raised above ZCode's 32 KiB default so a fetched handoff is not
        // truncated before injection.
        assert_eq!(payload["maxOutputBytes"], serde_json::json!(65536));
        let events = payload["events"].as_object().unwrap();
        assert_eq!(
            events.len(),
            super::super::render_shared::ZCODE_EVENTS.len(),
            "one entry per documented ZCode trigger"
        );
        for (zcode_event, our_event) in super::super::render_shared::ZCODE_EVENTS {
            let groups = events[zcode_event].as_array().unwrap();
            assert_eq!(groups.len(), 1, "{zcode_event}: one matcher group");
            let hooks = groups[0]["hooks"].as_array().unwrap();
            assert_eq!(hooks.len(), 1, "{zcode_event}: one hook entry");
            let hook = &hooks[0];
            // Exec form: `type: "process"` spawns command + args with no
            // shell, so every key must be from ZCode's documented schema —
            // undocumented keys make it drop the entry.
            assert_eq!(hook["type"], serde_json::json!("process"), "{zcode_event}");
            assert_eq!(
                hook["statusMessage"].as_str().unwrap(),
                "ai-memory capture",
                "{zcode_event}: ownership marker"
            );
            assert_eq!(hook["timeoutMs"], serde_json::json!(10_000));
            for key in hook.as_object().unwrap().keys() {
                assert!(
                    matches!(
                        key.as_str(),
                        "type" | "command" | "args" | "enabled" | "timeoutMs" | "statusMessage"
                    ),
                    "{zcode_event}: undocumented hook key {key}"
                );
            }
            let args: Vec<&str> = hook["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect();
            assert!(args.contains(&"hook"), "{args:?}");
            assert!(
                args.contains(&"--event") && args.contains(&our_event),
                "{args:?}"
            );
            assert!(
                args.contains(&"--agent") && args.contains(&"zcode"),
                "{args:?}"
            );
            assert!(args.contains(&"--server-url") && args.contains(&"http://127.0.0.1:49374"));
            assert!(args.contains(&"--auth-token") && args.contains(&"tok-test"));
            assert!(args.contains(&"--data-dir") && args.contains(&"/data"));
            assert!(args.contains(&"--project-strategy") && args.contains(&"repo-root"));
        }
        // PostToolUseFailure fires instead of PostToolUse when the tool
        // throws; the loop above already proved it forwards onto the same
        // `post-tool-use` channel as its success sibling.
        assert!(events.contains_key("PostToolUseFailure"));
    }

    #[test]
    fn zcode_apply_preserves_siblings_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        fs::write(
            &path,
            r#"{
  "theme": "dark",
  "model": {"main": "zai/glm-5.1"},
  "mcp": {"servers": {"other": {"type": "http", "url": "https://other.example/mcp"}}},
  "hooks": {
    "enabled": true,
    "events": {
      "SessionStart": [
        {"matcher": "Bash", "hooks": [
          {"type": "process", "command": "/usr/bin/true", "args": [], "enabled": true}
        ]},
        {"hooks": [
          {"type": "process", "command": "/old/ai-memory", "args": [],
           "enabled": true, "statusMessage": "ai-memory capture"}
        ]}
      ]
    }
  }
}"#,
        )
        .unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::Zcode,
            capture_assistant: false,
            config_file: Some(path.clone()),
            ..default_hook_args()
        };

        apply_to_zcode_hooks(
            "http://127.0.0.1:49374",
            Some("tok-test"),
            Path::new("/data"),
            &args,
        )
        .unwrap();
        let first = fs::read_to_string(&path).unwrap();
        apply_to_zcode_hooks(
            "http://127.0.0.1:49374",
            Some("tok-test"),
            Path::new("/data"),
            &args,
        )
        .unwrap();
        let second = fs::read_to_string(&path).unwrap();

        assert_eq!(first, second, "re-apply must be a no-op");
        let root: serde_json::Value = serde_json::from_str(&second).unwrap();
        // Sibling config keys survive: hooks share the file with the model
        // and MCP registration (#511).
        assert_eq!(root["theme"], serde_json::json!("dark"));
        assert_eq!(root["model"]["main"], serde_json::json!("zai/glm-5.1"));
        assert_eq!(
            root["mcp"]["servers"]["other"]["url"],
            serde_json::json!("https://other.example/mcp")
        );
        // The handoff-injection stdout ceiling lands with the hooks block.
        assert_eq!(root["hooks"]["maxOutputBytes"], serde_json::json!(65536));
        let session_start = root["hooks"]["events"]["SessionStart"].as_array().unwrap();
        assert_eq!(
            session_start.len(),
            2,
            "the third-party matcher group must survive the merge"
        );
        let third_party = &session_start[0];
        assert_eq!(
            third_party["matcher"],
            serde_json::json!("Bash"),
            "third-party matcher groups are preserved untouched"
        );
        assert_eq!(
            third_party["hooks"][0]["command"],
            serde_json::json!("/usr/bin/true")
        );
        let ours = &session_start[1];
        assert_eq!(
            ours["hooks"][0]["command"],
            super::super::render_shared::build_zcode_hooks_config(
                "http://127.0.0.1:49374",
                Some("tok-test"),
                Some(Path::new("/data")),
                None
            )["events"]["SessionStart"][0]["hooks"][0]["command"],
            "the stale ai-memory command path must be replaced"
        );
        for (zcode_event, _) in super::super::render_shared::ZCODE_EVENTS {
            assert!(
                root["hooks"]["events"][zcode_event].is_array(),
                "missing {zcode_event}"
            );
        }
    }
    #[test]
    fn zcode_apply_withdraws_our_hooks_from_foreign_keys_and_keeps_the_rest() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        // Keys outside `ZCODE_EVENTS`. ZCode rejects the whole block over
        // ones it does not recognize, which is how #600 stayed invisible.
        // `SessionEnd` runs nothing, `PreCompact` holds only our entry, and
        // `Notification` mixes our entry with a hook we did not write.
        fs::write(
            &path,
            r#"{
  "hooks": {
    "enabled": true,
    "events": {
      "SessionEnd": [],
      "PreCompact": [
        {"hooks": [
          {"type": "process", "command": "/old/ai-memory", "args": [],
           "enabled": true, "statusMessage": "ai-memory capture"}
        ]}
      ],
      "Notification": [
        {"hooks": [
          {"type": "process", "command": "/old/ai-memory", "args": [],
           "enabled": true, "statusMessage": "ai-memory capture"},
          {"type": "process", "command": "/usr/bin/true", "args": [], "enabled": true}
        ]}
      ]
    }
  }
}"#,
        )
        .unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::Zcode,
            capture_assistant: false,
            config_file: Some(path.clone()),
            ..default_hook_args()
        };

        apply_to_zcode_hooks(
            "http://127.0.0.1:49374",
            Some("tok-test"),
            Path::new("/data"),
            &args,
        )
        .unwrap();

        let first = fs::read_to_string(&path).unwrap();
        let root: serde_json::Value = serde_json::from_str(&first).unwrap();
        let events = root["hooks"]["events"].as_object().unwrap();

        // Event keys are never deleted: they are not ai-memory's to remove.
        assert!(events.contains_key("SessionEnd"));
        assert!(events.contains_key("PreCompact"));
        // Our own stale entry is withdrawn wherever it sits.
        assert_eq!(
            events["PreCompact"][0]["hooks"].as_array().unwrap().len(),
            0,
            "our entry under a key we no longer write must be withdrawn"
        );
        // The mixed group is the only case where the ownership filter
        // decides anything: the foreign hook survives, ours does not.
        let notification = events["Notification"][0]["hooks"].as_array().unwrap();
        assert_eq!(notification.len(), 1, "the hook we did not write must stay");
        assert_eq!(
            notification[0]["command"],
            serde_json::json!("/usr/bin/true")
        );
        for (zcode_event, _) in super::super::render_shared::ZCODE_EVENTS {
            assert!(
                root["hooks"]["events"][zcode_event].is_array(),
                "missing {zcode_event}"
            );
        }

        // Re-apply is still a no-op once the withdrawal has happened.
        apply_to_zcode_hooks(
            "http://127.0.0.1:49374",
            Some("tok-test"),
            Path::new("/data"),
            &args,
        )
        .unwrap();
        assert_eq!(first, fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn zcode_event_runs_nothing_covers_degenerate_matcher_groups() {
        // Each of these leaves the key present but unable to run anything,
        // which is what makes ZCode reject the block while the file still
        // looks settled (#600).
        for shape in [
            serde_json::json!([]),
            serde_json::json!([{}]),
            serde_json::json!([{"matcher": "*"}]),
            serde_json::json!([{"hooks": []}]),
            serde_json::json!([{"hooks": null}]),
        ] {
            assert!(
                zcode_event_runs_nothing(&shape),
                "expected {shape} to run nothing"
            );
        }
        assert!(!zcode_event_runs_nothing(&serde_json::json!([
            {"hooks": [{"type": "process", "command": "/usr/bin/true"}]}
        ])));
        // A value that is not a matcher-group array cannot run hooks either,
        // and is doubly invalid to ZCode. It is left unmodified, but it is
        // still reported: staying quiet about it is the #600 failure.
        assert!(zcode_event_runs_nothing(&serde_json::json!(42)));
    }

    #[test]
    fn zcode_apply_flags_a_user_disabled_hooks_block() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        fs::write(&path, r#"{"hooks": {"enabled": false, "events": {}}}"#).unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::Zcode,
            capture_assistant: false,
            config_file: Some(path.clone()),
            ..default_hook_args()
        };

        apply_to_zcode_hooks("http://127.0.0.1:49374", None, Path::new("/data"), &args).unwrap();

        let root: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            root["hooks"]["enabled"],
            serde_json::json!(false),
            "a user-disabled hooks block must stay disabled (we warn instead)"
        );
        assert_eq!(
            root["hooks"]["events"].as_object().unwrap().len(),
            super::super::render_shared::ZCODE_EVENTS.len()
        );
    }

    #[test]
    fn zcode_apply_respects_a_user_chosen_output_ceiling() {
        // `maxOutputBytes` is block-level and would also bound third-party
        // hooks, so a value the user chose is never overwritten.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        fs::write(
            &path,
            r#"{"hooks": {"enabled": true, "maxOutputBytes": 4096, "events": {}}}"#,
        )
        .unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::Zcode,
            capture_assistant: false,
            config_file: Some(path.clone()),
            ..default_hook_args()
        };

        apply_to_zcode_hooks("http://127.0.0.1:49374", None, Path::new("/data"), &args).unwrap();

        let root: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["hooks"]["maxOutputBytes"], serde_json::json!(4096));
    }

    #[test]
    fn opencode_mcp_inference_supplies_hook_origin_and_token() {
        let inferred = infer_json_mcp_config(
            r#"{
              "mcp": {
                "ai-memory": {
                  "type": "remote",
                  "url": "http://homelab:49374/mcp",
                  "headers": { "Authorization": "Bearer secret-token" }
                }
              }
            }"#,
            &["mcp", "ai-memory"],
            "url",
        )
        .unwrap();

        assert_eq!(
            inferred.hook_server_url.as_deref(),
            Some("http://homelab:49374")
        );
        assert_eq!(inferred.auth_token.as_deref(), Some("secret-token"));
    }

    /// Inferring the hook URL from Kimi Code's flavored mcp.json entry
    /// must yield the bare origin — hooks POST to `<origin>/hook`.
    #[test]
    fn hook_server_url_from_mcp_url_strips_query_and_suffix() {
        for (input, expected) in [
            ("http://homelab:49374/mcp", Some("http://homelab:49374")),
            ("http://homelab:49374", Some("http://homelab:49374")),
            ("http://homelab:49374/", Some("http://homelab:49374")),
            (
                "http://homelab:49374/mcp?flavor=moonshot",
                Some("http://homelab:49374"),
            ),
            (
                "http://homelab:49374/mcp/?flavor=moonshot",
                Some("http://homelab:49374"),
            ),
            // Reverse-proxy prefix survives; hooks POST under it.
            (
                "http://homelab:49374/wiki/mcp",
                Some("http://homelab:49374/wiki"),
            ),
            (
                "http://homelab:49374/wiki/mcp?flavor=moonshot",
                Some("http://homelab:49374/wiki"),
            ),
            ("", None),
            ("   ", None),
        ] {
            assert_eq!(
                hook_server_url_from_mcp_url(input).as_deref(),
                expected,
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn bundled_posix_and_powershell_hooks_stay_in_parity() {
        let hooks_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("hooks");
        assert!(
            hooks_root.join("lib").join("ai-memory-hook.ps1").is_file(),
            "PowerShell hooks require the shared lib helper"
        );

        // Enumerate the bundles instead of listing them: a hardcoded
        // list silently skips whatever agent lands next, which is how
        // `command-code` and `kiro-cli` shipped uncovered. `lib/` holds
        // the shared PowerShell helper, not an agent bundle.
        let mut agent_dirs: Vec<String> = fs::read_dir(&hooks_root)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", hooks_root.display()))
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .filter_map(|path| path.file_name()?.to_str().map(str::to_string))
            .filter(|name| name != "lib")
            .collect();
        agent_dirs.sort();
        assert!(
            !agent_dirs.is_empty(),
            "no agent hook bundles found under {}",
            hooks_root.display()
        );

        for agent_dir in agent_dirs {
            let agent_dir = agent_dir.as_str();
            let dir = hooks_root.join(agent_dir);
            let mut sh = BTreeMap::new();
            let mut ps1 = BTreeMap::new();
            for entry in fs::read_dir(&dir).unwrap_or_else(|e| {
                panic!("failed to read bundled hook dir {}: {e}", dir.display())
            }) {
                let path = entry.unwrap().path();
                if !path.is_file() {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                match path.extension().and_then(|s| s.to_str()) {
                    Some("sh") => {
                        sh.insert(stem.to_string(), extract_sh_hook_metadata(&path));
                    }
                    Some("ps1") => {
                        ps1.insert(stem.to_string(), extract_ps1_hook_metadata(&path));
                    }
                    _ => {}
                }
            }
            assert_eq!(
                sh.keys().collect::<Vec<_>>(),
                ps1.keys().collect::<Vec<_>>(),
                "{agent_dir}: every .sh hook must have a .ps1 peer"
            );
            for (stem, sh_meta) in sh {
                assert_eq!(
                    Some(sh_meta),
                    ps1.remove(&stem),
                    "{agent_dir}/{stem}: .sh and .ps1 must post the same event/agent"
                );
            }
        }
    }

    fn extract_sh_hook_metadata(path: &Path) -> (String, String) {
        let text = fs::read_to_string(path).unwrap();
        let marker = "hook?event=";
        let start = text
            .find(marker)
            .unwrap_or_else(|| panic!("{} missing hook endpoint", path.display()))
            + marker.len();
        let rest = &text[start..];
        let event = rest
            .split('&')
            .next()
            .unwrap_or_else(|| panic!("{} missing event", path.display()))
            .to_string();
        let agent_marker = "&agent=";
        let agent_start = rest
            .find(agent_marker)
            .unwrap_or_else(|| panic!("{} missing agent", path.display()))
            + agent_marker.len();
        let agent = rest[agent_start..]
            .split(['"', '\'', ' ', '\n', '\r', '$'])
            .next()
            .unwrap_or_else(|| panic!("{} missing agent value", path.display()))
            .to_string();
        (event, agent)
    }

    fn extract_ps1_hook_metadata(path: &Path) -> (String, String) {
        let text = fs::read_to_string(path).unwrap();
        let line = text
            .lines()
            .find(|line| line.contains("Invoke-AiMemoryHook"))
            .unwrap_or_else(|| panic!("{} missing Invoke-AiMemoryHook", path.display()));
        (
            extract_ps1_arg(line, "Event", path),
            extract_ps1_arg(line, "Agent", path),
        )
    }

    fn extract_ps1_arg(line: &str, name: &str, path: &Path) -> String {
        let marker = format!("-{name} \"");
        let start = line
            .find(&marker)
            .unwrap_or_else(|| panic!("{} missing {name} argument", path.display()))
            + marker.len();
        line[start..]
            .split('"')
            .next()
            .unwrap_or_else(|| panic!("{} missing {name} value", path.display()))
            .to_string()
    }

    // ----------------------------------------------------------------
    // Shared `_lib.sh` staging
    // ----------------------------------------------------------------

    /// `stage_hook_scripts` copies the parent dir's `_lib.sh` alongside
    /// the agent's event scripts so the runtime layout doesn't depend
    /// on the source-tree shape. This is the only piece of evidence we
    /// have that the marker-file walk-up helper actually ships — the
    /// scripts themselves source it with `. "$(dirname "$0")/_lib.sh"`
    /// and a missing helper would surface as a runtime "command not
    /// found" much further from the cause.
    /// #581: inside the docker wrapper the server data dir is a volume
    /// the host cannot execute from; staging must fall back to the
    /// home-based path the wrapper bind-mounts and reads. Natively, the
    /// resolved data dir keeps winning (#554/#573).
    #[test]
    fn staging_root_leaves_the_container_volume_for_home() {
        let data = Path::new("/data");
        assert_eq!(
            hook_staging_root(data, false),
            PathBuf::from("/data"),
            "native installs stage under the resolved data dir"
        );
        let in_container = hook_staging_root(data, true);
        assert_ne!(
            in_container,
            PathBuf::from("/data"),
            "container staging must not target the volume"
        );
        assert_eq!(
            in_container,
            dirs::data_local_dir().unwrap().join("ai-memory"),
            "container staging targets the wrapper's bind-mounted home contract"
        );
    }

    #[test]
    fn stage_hook_scripts_copies_shared_lib_sh() {
        // Distinct agent_label per test: `stage_hook_scripts` writes
        // under `dirs::data_local_dir()/.../hooks/<agent_label>` and
        // the test binary runs cases in parallel, so two tests using
        // the same label race on the same staging dir.
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("hooks");
        let agent_src = bundle.join("stage-shared-lib");
        fs::create_dir_all(&agent_src).unwrap();
        fs::write(bundle.join("_lib.sh"), "# shared helper\n").unwrap();
        stub_scripts(&agent_src, &["session-start.sh", "post-tool-use.sh"]);

        let data_dir = tmp.path().join("data");
        let staged = stage_hook_scripts_in(&agent_src, "stage-shared-lib", &data_dir).unwrap();
        assert!(staged.join("session-start.sh").exists());
        assert!(staged.join("post-tool-use.sh").exists());
        assert!(
            staged.join("_lib.sh").exists(),
            "_lib.sh must be staged alongside event scripts",
        );

        let lib = fs::read_to_string(staged.join("_lib.sh")).unwrap();
        assert!(
            lib.contains("shared helper"),
            "staged _lib.sh must match the source-of-truth"
        );
    }

    /// Skipping `_lib.sh` is fine — older source bundles without the
    /// marker-walk-up feature should still install cleanly.
    #[test]
    fn stage_hook_scripts_tolerates_missing_lib_sh() {
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("hooks");
        let agent_src = bundle.join("stage-no-lib");
        fs::create_dir_all(&agent_src).unwrap();
        // Note: no _lib.sh in `bundle`.
        stub_scripts(&agent_src, &["session-start.sh"]);

        let data_dir = tmp.path().join("data");
        let staged = stage_hook_scripts_in(&agent_src, "stage-no-lib", &data_dir).unwrap();
        assert!(staged.join("session-start.sh").exists());
        assert!(!staged.join("_lib.sh").exists());
    }

    /// Regression for issue #52 — when `resolve_hooks_dir` picks the
    /// data-local dir as the source bundle (the docker `setup-agent`
    /// flow extracts scripts there) AND the staging destination is
    /// the *same* dir, the pre-fix wipe-then-copy loop would delete
    /// every populated script and report `staged 0`. The same-path
    /// branch must verify in place without wiping, so existing scripts
    /// survive a re-run.
    #[test]
    fn stage_hook_scripts_preserves_in_place_scripts_when_source_equals_dest() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let agent_label = "stage-in-place";
        // Simulate "scripts already extracted into the data dir's hooks
        // dir by a prior `setup-agent` run".
        let in_place = data_dir.join("hooks").join(agent_label);
        fs::create_dir_all(&in_place).unwrap();
        stub_scripts(&in_place, &["session-start.sh", "post-tool-use.sh"]);

        // Source == destination (this is what resolve_hooks_dir hands
        // us when no other candidate exists).
        let staged = stage_hook_scripts_in(&in_place, agent_label, &data_dir).unwrap();

        assert_eq!(staged, in_place, "destination must canonicalize to source");
        assert!(
            staged.join("session-start.sh").is_file(),
            "in-place script must survive the same-path branch (not be wiped)"
        );
        assert!(
            staged.join("post-tool-use.sh").is_file(),
            "in-place script must survive the same-path branch (not be wiped)"
        );
    }

    /// Regression for issue #52 — the failure that the reporter actually
    /// hit: `resolve_hooks_dir` resolved to a pre-existing but empty
    /// data-local dir, so source == dest and there's nothing to verify.
    /// The pre-fix code silently returned Ok with `copied = 0` and the
    /// caller went on to rewrite `settings.json` against an empty dir,
    /// disabling capture without any error. We must bail with an
    /// actionable message instead.
    #[test]
    fn stage_hook_scripts_bails_when_source_equals_empty_dest() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let agent_label = "stage-empty-in-place";
        // Must equal what `stage_hook_scripts_in` computes, or this stops
        // being the source==dest case it exists to cover and duplicates the
        // different-paths test below (#554 moved the destination shape).
        let in_place = data_dir.join("hooks").join(agent_label);
        fs::create_dir_all(&in_place).unwrap();
        // Intentionally no scripts in `in_place`.

        let err = stage_hook_scripts_in(&in_place, agent_label, &data_dir)
            .expect_err("an empty source dir must produce a hard error, not Ok(0)");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no hook scripts"),
            "error should call out the empty source: {msg}"
        );
        assert!(
            msg.contains("--hooks-dir") || msg.contains("setup-agent"),
            "error should point at the workaround (--hooks-dir or setup-agent): {msg}"
        );
    }

    /// Regression for issue #52 — same fail-on-zero guard applies even
    /// when source and dest are different paths (e.g. user pointed
    /// `--hooks-dir` at the wrong dir). Previously this also silently
    /// returned Ok with `copied = 0`.
    #[test]
    fn stage_hook_scripts_bails_when_source_dir_is_empty() {
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("hooks");
        let agent_src = bundle.join("stage-empty-src");
        fs::create_dir_all(&agent_src).unwrap();
        // Source dir exists but has no scripts.

        let data_dir = tmp.path().join("data");
        let err = stage_hook_scripts_in(&agent_src, "stage-empty-src", &data_dir)
            .expect_err("zero scripts should be an error, not a silent success");
        assert!(format!("{err:#}").contains("no hook scripts"));
    }

    /// #554: with `AI_MEMORY_DATA_DIR` set, `install-hooks` probed and staged
    /// under the *platform default* instead of the data dir in use — so the
    /// server logged one path while `--apply` reported staging into another,
    /// silently populating a directory the operator was not using.
    #[test]
    fn a_configured_data_dir_is_where_the_bundle_is_probed_and_staged() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("custom-data");
        let source = tmp.path().join("bundle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("session-start.sh"), b"#!/bin/sh\n").unwrap();

        // Staged into the configured dir…
        let staged = stage_hook_scripts_in(&source, "claude-code", &data_dir).unwrap();
        assert_eq!(staged, data_dir.join("hooks").join("claude-code"));
        assert!(staged.join("session-start.sh").is_file());
        assert!(
            !tmp.path().join("ai-memory").exists(),
            "nothing may be written under a platform-default path"
        );

        // …and the probe looks in the same place, so the two halves agree.
        let candidates = hook_source_candidates("claude-code", None, None, Some(data_dir.clone()));
        assert!(
            candidates.contains(&data_dir.join("hooks").join("claude-code")),
            "the configured data dir must be probed; got {candidates:?}"
        );
    }

    /// The default install must be byte-identical to the old behaviour:
    /// `default_data_dir()` is `data_local_dir()/ai-memory`, so
    /// `<data_dir>/hooks/<agent>` is the same path the previous
    /// `<data_local>/ai-memory/hooks/<agent>` produced.
    #[test]
    fn the_default_data_dir_stages_to_the_same_path_as_before() {
        let tmp = TempDir::new().unwrap();
        let data_local = tmp.path().join(".local").join("share");
        let default_data_dir = data_local.join("ai-memory");
        let source = tmp.path().join("bundle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("session-start.sh"), b"#!/bin/sh\n").unwrap();

        let staged = stage_hook_scripts_in(&source, "claude-code", &default_data_dir).unwrap();
        assert_eq!(
            staged,
            data_local
                .join("ai-memory")
                .join("hooks")
                .join("claude-code"),
            "existing installs must keep staging exactly where they did"
        );
    }

    /// #552, the regression guard: `--apply` must persist the bearer under the
    /// data dir and leave it out of the agent config entirely.
    ///
    /// Before this, the token was written into the rendered command — as
    /// `--auth-token <token>` for native hooks — so it sat in the agent's
    /// config file *and* in `/proc/<pid>/cmdline` for the lifetime of every
    /// hook, on every tool call.
    #[test]
    fn apply_persists_the_bearer_and_keeps_it_out_of_the_agent_config() {
        let home = TempDir::new().unwrap();
        let cfg_dir = TempDir::new().unwrap();
        let settings = cfg_dir.path().join("settings.json");
        std::fs::write(&settings, "{}").unwrap();

        let config = crate::config::Config::load(None, Some(home.path().to_path_buf())).unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::ClaudeCode,
            apply: true,
            server_url: Some("http://127.0.0.1:49374".to_string()),
            auth_token: Some("SEKRIT-BEARER-552".to_string()),
            config_file: Some(settings.clone()),
            // Point at the repo's bundle explicitly. Without this the test
            // leans on hook-dir discovery, which from `target/debug/deps`
            // walks to `<repo>/target` and then falls through to whatever the
            // machine happens to have staged — passing for a reason that has
            // nothing to do with what is under test.
            hooks_dir: Some(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks"),
            ),
            ..default_hook_args()
        };

        run(&config, args).expect("apply should succeed");

        let rendered = std::fs::read_to_string(&settings).unwrap();
        assert!(
            !rendered.contains("SEKRIT-BEARER-552"),
            "the bearer must not be written into the agent config: {rendered}"
        );
        assert!(
            !rendered.contains("--auth-token"),
            "no --auth-token flag may reach a hook command line: {rendered}"
        );

        assert_eq!(
            crate::config::read_hook_auth_token(&config.data_dir).as_deref(),
            Some("SEKRIT-BEARER-552"),
            "the bearer must be persisted where the hook can read it"
        );
        assert!(
            std::fs::read_to_string(crate::config::hook_auth_header_path_in(&config.data_dir))
                .unwrap()
                .contains("SEKRIT-BEARER-552"),
            "the curl header file must carry it too"
        );
    }

    #[test]
    fn hook_source_candidates_include_native_package_dir() {
        let candidates = hook_source_candidates(
            "claude-code",
            Some(PathBuf::from("/repo")),
            Some(PathBuf::from("/opt/ai-memory")),
            // The resolved data dir, not the platform data-local root (#554).
            Some(PathBuf::from("/home/alice/.local/share/ai-memory")),
        );

        assert_eq!(candidates[0], PathBuf::from("/repo/hooks/claude-code"));
        assert_eq!(
            candidates[1],
            PathBuf::from("/opt/ai-memory/hooks/claude-code")
        );
        assert_eq!(
            candidates[2],
            PathBuf::from("/usr/local/share/ai-memory/hooks/claude-code")
        );
        assert_eq!(
            candidates[3],
            PathBuf::from("/usr/share/ai-memory/hooks/claude-code")
        );
        assert_eq!(
            candidates[4],
            PathBuf::from("/home/alice/.local/share/ai-memory/hooks/claude-code")
        );
    }

    /// #546: `~/.local/bin/ai-memory -> <repo>/target/release/ai-memory` is
    /// what the quick start's `~/.local/bin` layout produces for a source
    /// build. Discovery is derived from the binary's own location, so the
    /// link has to be resolved first or every candidate misses the checkout.
    #[cfg(unix)]
    #[test]
    fn hooks_are_found_through_a_symlinked_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let exe = repo.join("target").join("release").join("ai-memory");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"").unwrap();
        let link_dir = tmp.path().join("home").join(".local").join("bin");
        fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join("ai-memory");
        std::os::unix::fs::symlink(&exe, &link).unwrap();

        let resolved = resolve_exe_path(link);
        let candidates = hook_source_candidates(
            "claude-code",
            repo_root_from_exe(&resolved),
            resolved.parent().map(Path::to_path_buf),
            None,
        );

        assert_eq!(
            candidates[0],
            fs::canonicalize(&repo)
                .unwrap()
                .join("hooks")
                .join("claude-code"),
            "the checkout must be the first candidate, not the symlink's ancestor"
        );
    }

    #[test]
    fn resolve_exe_path_keeps_an_unresolvable_path() {
        let missing = PathBuf::from("/definitely/not/here/ai-memory");
        assert_eq!(resolve_exe_path(missing.clone()), missing);
    }

    #[test]
    fn hook_source_candidates_include_binary_sibling_for_flat_tarball() {
        // Extracted release tarball: no repo root, `hooks/` beside the binary
        // (issue #107). The sibling dir must be probed or discovery fails with
        // a bogus `/private/hooks/...` on macOS.
        let candidates = hook_source_candidates(
            "claude-code",
            None,
            Some(PathBuf::from("/private/tmp/ai-memory-macos-aarch64")),
            None,
        );
        assert!(
            candidates.contains(&PathBuf::from(
                "/private/tmp/ai-memory-macos-aarch64/hooks/claude-code"
            )),
            "binary-sibling hooks/ dir must be probed; got {candidates:?}"
        );
    }

    // ----------------------------------------------------------------
    // OpenCode tests
    // ----------------------------------------------------------------

    fn assert_generated_ts_uses_bounded_hook_queue(generated: &str) {
        assert!(generated.contains("const HOOK_QUEUE_MAX = 100;"));
        assert!(generated.contains("const HOOK_FLUSH_INTERVAL_MS = 2000;"));
        assert!(generated.contains("const HOOK_FLUSH_THRESHOLD = 20;"));
        assert!(generated.contains("const HOOK_INTER_REQUEST_DELAY_MS = 50;"));
        assert!(generated.contains("const HOOK_REQUEST_TIMEOUT_MS = 2000;"));
        assert!(generated.contains("const HOOK_IMMEDIATE_EVENTS = new Set([\"session-start\", \"stop\", \"session-end\", \"pre-compact\"]);"));
        assert!(generated.contains("const hookQueue: HookQueueItem[] = [];"));
        assert!(generated.contains(
            "function enqueueHook(event: string, url: URL, payload: Record<string, unknown>): void"
        ));
        assert!(generated.contains("if (hookQueue.length >= HOOK_QUEUE_MAX) hookQueue.shift();"));
        assert!(generated.contains(
            "HOOK_IMMEDIATE_EVENTS.has(event) || hookQueue.length >= HOOK_FLUSH_THRESHOLD"
        ));
        assert!(generated.contains("function scheduleHookFlush(): void"));
        assert!(generated.contains("hookFlushTimer.unref?.();"));
        assert!(generated.contains("async function drainHookQueue(): Promise<void>"));
        assert!(generated.contains("signal: timeoutSignal(HOOK_REQUEST_TIMEOUT_MS)"));
        assert!(generated.contains("await sleep(HOOK_INTER_REQUEST_DELAY_MS)"));
        assert!(generated.contains("const policy = capturePolicy(payload"));
        assert!(generated.contains("if (policy.disposition === \"drop\") return;"));
        assert!(generated.contains("enqueueHook(event, url, policy.payload);"));
        assert!(generated.contains("const CAPTURE_POLICY_V1 = 1;"));
        assert!(generated.contains("const CAPTURE_MARKER_MAX_BYTES = 64 * 1024;"));
        assert!(generated.contains("async function fetchHandoff"));
        assert!(generated.contains("if (!process.env.AI_MEMORY_RUN_ID) {"));
        assert!(generated.contains("const response = await fetch(url, {"));
        assert!(generated.contains("signal: timeoutSignal(1000)"));
        assert!(!generated.contains("signal: timeoutSignal(500)"));
        assert!(!generated.contains("void fetch(url, {"));
    }

    #[test]
    fn generated_ts_integrations_spool_failed_deliveries() {
        // #580: every queue-based TS integration must persist a failed
        // delivery instead of dropping it, and must self-drain the spool.
        for (name, source) in [
            (
                "opencode",
                build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist"),
            ),
            (
                "opencode2",
                build_opencode2_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist"),
            ),
        ] {
            // The fire-and-forget delivery must be gone...
            assert!(
                !source.contains("}).catch(() => undefined);\n      } catch (_e) {"),
                "{name}: still fire-and-forgets failed deliveries"
            );
            // ...replaced by capture + spool of network failures and 5xx.
            assert!(
                source.contains(
                    "if (!resp || resp.status >= 500) spoolFailedHook(item.url, item.payload);"
                ),
                "{name}: missing spool-on-failure"
            );
            // The spool runtime is present and CLI-compatible.
            assert!(source.contains("function spoolFailedHook("), "{name}");
            assert!(source.contains("async function drainHookSpool()"), "{name}");
            assert!(
                source.contains(r#"return join(env, "hook-spool");"#),
                "{name}: spool dir must honour AI_MEMORY_DATA_DIR"
            );
            // Every queue flush also kicks a spool drain.
            assert!(
                source.contains("if (hookDraining) return;\n  requestSpoolDrain();"),
                "{name}: drainHookQueue must trigger the spool drain"
            );
            // The runtime's fs needs made it into the import line.
            for f in [
                "mkdirSync",
                "writeFileSync",
                "renameSync",
                "readdirSync",
                "unlinkSync",
            ] {
                assert!(
                    source.contains(&format!("{f},")) || source.contains(&format!("{f} }}")),
                    "{name}: node:fs import missing {f}"
                );
            }
        }
    }

    #[test]
    fn ts_spool_entries_are_readable_by_the_cli_drain() {
        // The TS runtime writes the CLI hook-spool's exact on-disk contract.
        // Mirror the bytes the generated code produces and prove the CLI
        // side parses them: entry JSON deserializes, the filename's
        // timestamp is honoured, and the body survives round-tripping.
        let created_ms: u64 = 1_756_800_000_000;
        // Filename exactly as the TS builds it:
        // `${String(createdMs).padStart(13, "0")}-${process.pid}-${seq}.json`
        let name = format!("{created_ms:013}-4242-{:016x}.json", 7u64);
        // Entry JSON exactly as the TS `JSON.stringify` emits it (key order
        // irrelevant to serde, but the shapes and enum strings are load-bearing).
        let json = format!(
            r#"{{"url":"http://127.0.0.1:49374/hook?event=stop&agent=open-code&ingest_key=ts18f3","body":"{{\"session_id\":\"s1\"}}","created_ms":{created_ms},"auth_mode":"static","token":"tok","attempts":0}}"#
        );
        let entry: crate::commands::hook_spool::SpoolEntry =
            serde_json::from_str(&json).expect("CLI drain must parse a TS-written entry");
        assert_eq!(entry.created_ms, created_ms);
        assert_eq!(entry.token.as_deref(), Some("tok"));
        assert_eq!(entry.attempts, 0);
        assert!(entry.url.contains("ingest_key=ts18f3"));
        serde_json::from_str::<serde_json::Value>(&entry.body)
            .expect("TS body must be raw JSON, not double-encoded");

        // And the anonymous variant (no token installed).
        let anon = r#"{"url":"http://h/hook?event=stop&agent=opencode","body":"{}","created_ms":1,"auth_mode":"none","attempts":0}"#;
        serde_json::from_str::<crate::commands::hook_spool::SpoolEntry>(anon)
            .expect("auth_mode none must parse");

        // The timestamp-bearing filename is one the spool reader accepts:
        // spool_health derives oldest-age from names without reading files.
        let dir = tempfile::tempdir().expect("tempdir");
        let spool = dir.path().join("hook-spool");
        std::fs::create_dir_all(&spool).expect("mkdir");
        std::fs::write(spool.join(&name), json.as_bytes()).expect("write");
        assert_eq!(crate::commands::hook_spool::spool_len(&spool), 1);
        let health = crate::commands::hook_spool::spool_health(&spool);
        assert_eq!(health.pending, 1);
        assert!(
            health.oldest_age_ms.is_some(),
            "TS filename's 13-digit timestamp must be readable: {name}"
        );
    }

    #[test]
    fn opencode_plugin_uses_real_plugin_hooks() {
        let plugin = build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist");

        assert!(plugin.contains("event: async (input)"));
        assert!(plugin.contains(r#""chat.message": async"#));
        assert!(plugin.contains(r#""tool.execute.before": async"#));
        assert!(plugin.contains(r#""tool.execute.after": async"#));
        assert!(plugin.contains(r#""experimental.chat.system.transform": async"#));
        assert!(plugin.contains("export default AiMemoryHooks"));
        assert!(plugin.contains("const startedSessions = new Set<string>();"));
        assert!(
            plugin
                .contains("const handoffFetches = new Map<string, Promise<string | undefined>>();")
        );
        assert!(!plugin.contains("handoffChecked"));
        assert!(plugin.contains("function startSession"));
        assert!(plugin.contains("function endSession"));
        assert!(plugin.contains("fetchHandoff"));
        assert!(plugin.contains("function applyMarkerParams"));
        assert!(plugin.contains("readFileSync(marker, \"utf8\")"));
        assert!(plugin.contains("text.split(/\\r?\\n/)"));
        assert!(plugin.contains("tomlKey(body, \"project_strategy\")"));
        assert!(plugin.contains("tomlKey(body, \"drop_subagent_captures\")"));
        assert!(plugin.contains("url.searchParams.set(\"project_strategy\", projectStrategy)"));
        assert!(plugin.contains("url.searchParams.set(\"drop_subagent\", dropSubagent)"));
        assert!(plugin.contains("function tomlFlag"));
        assert!(plugin.contains("tomlFlag(body, \"default_global\")"));
        assert!(plugin.contains("tomlFlag(body, \"inject_on_session_start\")"));
        assert!(plugin.contains("url.searchParams.set(\"briefing_budget\", briefingBudget)"));
        // #668: applyMarkerParams resolves scope/settings via the
        // settings-walk, not the nearest-marker findMarker, so a nested
        // capture-only marker does not shadow an outer marker's scope.
        assert!(plugin.contains("function findSettingsMarker"));
        assert!(plugin.contains("function declaresSettings"));
        assert!(plugin.contains("const marker = findSettingsMarker(cwd);"));
        assert!(plugin.contains(
            "for (const key of [\"workspace\", \"project\", \"project_strategy\", \"drop_subagent_captures\"])"
        ));
        assert!(plugin.contains(
            "for (const key of [\"default_global\", \"inject_on_session_start\", \"max_chars\"])"
        ));
        assert!(
            plugin.contains("if (declaresSettings(readFileSync(marker, \"utf8\"))) return marker;")
        );
        assert!(plugin.contains(
            "applyMarkerParams(url, typeof payload.cwd === \"string\" ? payload.cwd : undefined);"
        ));
        assert!(plugin.contains("applyMarkerParams(url, cwd);"));
        assert!(plugin.contains("postPreCompact"));
        assert!(plugin.contains("dispose: async () =>"));
        assert!(plugin.contains("const HOOK_DISPOSE_DRAIN_BUDGET_MS = 2000;"));
        assert!(plugin.contains("let hookDrainPromise: Promise<void> | undefined;"));
        assert!(plugin.contains("function requestHookDrain(): Promise<void>"));
        assert!(plugin.contains("function disposeDrainTimeout(): Promise<void>"));
        assert!(plugin.contains("timer.unref?.();"));
        assert!(plugin.contains("async function drainHookQueueForDispose(): Promise<void>"));
        assert!(plugin.contains("for (const id of Array.from(startedSessions))"));
        assert!(plugin.contains("await drainHookQueueForDispose();"));
        assert!(plugin.contains("postHook(\"session-start\""));
        assert!(plugin.contains(r#""session.deleted")"#));
        assert_eq!(
            plugin.matches("postHook(\"session-end\"").count(),
            1,
            "OpenCode generated plugin must route session closes through one idempotent helper"
        );
        assert!(plugin.contains("!startedSessions.delete(id)"));
        assert!(plugin.contains("sessionCwds.delete(id);"));
        assert!(plugin.contains("handoffFetches.delete(id);"));
        assert!(plugin.contains("preCompactLast.delete(id);"));
        assert!(plugin.contains("postHook(\"user-prompt\""));
        assert!(plugin.contains("Bearer ${token}"));
        assert!(plugin.contains("tok"));
        assert!(
            !plugin.contains(r#""session.created": async"#),
            "OpenCode bus events must be handled through the `event` hook"
        );
        assert!(plugin.contains("import { execFileSync } from \"node:child_process\";"));
        assert!(
            plugin.contains("import { basename, dirname, join, resolve, sep } from \"node:path\";")
        );
        assert!(plugin.contains("if (existsSync(join(probe, \".git\")))"));
        assert!(plugin.contains("boundary ??= dir;"));
        assert!(plugin.contains("function repoRootProject"));
        assert!(plugin.contains("--git-common-dir"));
        assert!(
            plugin
                .contains("projectStrategy === \"repo-root\" || projectStrategy === \"repo_root\"")
        );
        assert!(plugin.contains("url.searchParams.set(\"project\", repoProject)"));
    }

    #[test]
    fn opencode2_plugin_binds_the_v2_api() {
        let plugin =
            build_opencode2_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist")
                .unwrap();

        // Ownership markers the uninstall gate keys on.
        assert!(plugin.contains("install-hooks --agent opencode2 --apply"));
        assert!(plugin.contains("const AGENT = \"opencode2\";"));
        // Type-only import: `Plugin.define` is identity, and a runtime
        // import does not resolve from the global plugins dir (the beta
        // refuses the load). The host only needs the `{ id, setup }` shape.
        assert!(plugin.contains("import type { Plugin } from \"@opencode-ai/plugin\";"));
        assert!(!plugin.contains("export default Plugin.define"));
        assert!(!plugin.contains("Plugin.define({"));
        assert!(plugin.contains("const AiMemoryOpencode2: Plugin = {"));
        assert!(plugin.contains("export default AiMemoryOpencode2;"));
        // Beta hooks, verified against `@opencode-ai/plugin@beta`.
        assert!(plugin.contains("ctx.event.subscribe"));
        assert!(plugin.contains("ctx.session.hook(\"prompt\""));
        assert!(plugin.contains("ctx.session.hook(\"context\""));
        assert!(plugin.contains("ctx.tool.hook(\"execute.before\""));
        assert!(plugin.contains("ctx.tool.hook(\"execute.after\""));
        // V2 lifecycle envelopes carry `{ type, data, location }`.
        for event in [
            "session.created",
            "session.idle",
            "session.deleted",
            "session.compacted",
        ] {
            assert!(plugin.contains(event), "missing {event}");
        }
        // Handoff injection moved off v1's removed experimental hook and
        // fires once per session (the context hook runs per model call).
        // System items are `{ type: "text", text }` objects on the V2 API, not strings.
        assert!(plugin.contains("handoffInjected"));
        assert!(plugin.contains("system.push({ type: \"text\", text: handoff })"));
        assert!(!plugin.contains("experimental."));
        // Hook registrations are disposed on unload so a reload cannot
        // leave stale callbacks capturing twice.
        assert!(plugin.contains("registration.dispose()"));
        // Pre-compaction arrives on its own start event; the completion
        // event stays as the consolidation trigger, like v1.
        assert!(plugin.contains("session.compaction.started"));
        // Tool failures share the channel with the reason preserved.
        assert!(plugin.contains("status === \"error\""));
        // No v1 remnants.
        assert!(!plugin.contains("export default AiMemoryHooks"));
        assert!(!plugin.contains("chat.message"));
        // Shared capture prelude survived the rewrite byte-identical.
        for shared in [
            "function startSession",
            "function endSession",
            "postHook(\"session-start\"",
            "postHook(\"user-prompt\"",
            "function applyMarkerParams",
            "requestSpoolDrain();",
        ] {
            assert!(plugin.contains(shared), "missing {shared}");
        }
        // #625 made the generated adapters resolve the bearer at runtime
        // (resolveToken() -> `${token}`) instead of the static `${TOKEN}`;
        // the opencode2 plugin derives from v1 so it inherits that.
        assert!(plugin.contains("Bearer ${token}"));
        assert!(plugin.contains("tok"));
    }

    #[test]
    fn opencode2_plugin_rejects_v1_template_drift() {
        // The builder rewrites the v1 template, so a v1 edit that moves the
        // binding anchor must fail loudly instead of shipping a hybrid.
        let plugin = build_opencode2_plugin(
            "http://127.0.0.1:49374/",
            None,
            Some("repo-root"),
            "denylist",
        )
        .unwrap();
        assert!(plugin.contains("const TOKEN: string | null = null;"));
        assert!(
            plugin.contains("const DEFAULT_PROJECT_STRATEGY = \"repo-root\";"),
            "strategy bake-through must survive the v2 rewrite"
        );
    }

    #[test]
    fn opencode2_anchor_rewrite_fails_loudly_on_drift() {
        let err =
            replace_opencode_anchor("no v1 anchors here", "missing", "x", "banner").unwrap_err();
        assert!(
            err.to_string().contains("banner"),
            "drift must name the moved anchor: {err:#}"
        );
    }

    #[test]
    fn opencode2_plugin_bakes_allowlist_admit_gate() {
        // opencode2 reuses v1's capture prelude verbatim (see
        // `build_opencode2_plugin`'s doc comment), so the gate must survive
        // the anchor rewrite into the beta's `{ id, setup }` host binding.
        let plugin =
            build_opencode2_plugin("http://127.0.0.1:49374", Some("tok"), None, "allowlist")
                .unwrap();
        assert!(
            plugin.contains("const CAPTURE_MODE: \"allowlist\" | \"denylist\" = \"allowlist\";"),
            "{plugin}"
        );
        assert!(plugin.contains(CAPTURE_ADMIT_GATE_TS), "{plugin}");
    }

    #[test]
    fn opencode2_plugin_denylist_bakes_inert_gate() {
        let plugin =
            build_opencode2_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist")
                .unwrap();
        assert!(
            plugin.contains("const CAPTURE_MODE: \"allowlist\" | \"denylist\" = \"denylist\";"),
            "{plugin}"
        );
        assert!(plugin.contains(CAPTURE_ADMIT_GATE_TS), "{plugin}");
    }

    #[test]
    fn opencode_plugin_normalizes_payloads_without_legacy_wrapper() {
        let plugin = build_opencode_plugin("http://127.0.0.1:49374/", None, None, "denylist");

        assert!(plugin.contains("const SERVER = \"http://127.0.0.1:49374/\".replace"));
        assert!(plugin.contains("const TOKEN: string | null = null;"));
        assert!(plugin.contains("sessionID: id,"));
        assert!(plugin.contains("cwd,"));
        assert!(plugin.contains("prompt: textFromParts"));
        assert!(plugin.contains("output: (output as any).output"));
        assert!(plugin.contains("if (typeof AbortSignal === \"undefined\")"));
        assert!(
            !plugin.contains("hook_event_name"),
            "new plugin should send normalized top-level fields, not legacy wrappers"
        );
    }

    #[test]
    fn opencode_plugin_bakes_repo_root_default() {
        let plugin = build_opencode_plugin(
            "http://127.0.0.1:49374",
            Some("tok"),
            Some("repo-root"),
            "denylist",
        );
        assert!(
            plugin.contains("const DEFAULT_PROJECT_STRATEGY = \"repo-root\";"),
            "repo-root install default must bake the const: {plugin}"
        );
        assert!(
            plugin.contains("if (!projectStrategy) projectStrategy = DEFAULT_PROJECT_STRATEGY;"),
            "must apply the default when a marker pins no strategy: {plugin}"
        );
        assert!(
            plugin.contains("if (repoProject) project = repoProject;"),
            "{plugin}"
        );
        assert!(
            plugin.contains("const marker = findSettingsMarker(cwd);"),
            "the default-strategy variant must also walk past a capture-only marker (#668): {plugin}"
        );
    }

    #[test]
    fn opencode_plugin_default_omits_baked_strategy() {
        let plugin = build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist");
        assert!(
            !plugin.contains("DEFAULT_PROJECT_STRATEGY"),
            "basename default must bake no strategy: {plugin}"
        );
    }

    #[test]
    fn opencode_plugin_bakes_allowlist_admit_gate() {
        let plugin =
            build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None, "allowlist");
        assert!(
            plugin.contains("const CAPTURE_MODE: \"allowlist\" | \"denylist\" = \"allowlist\";"),
            "{plugin}"
        );
        assert!(plugin.contains(CAPTURE_ADMIT_GATE_TS), "{plugin}");
    }

    #[test]
    fn opencode_plugin_denylist_bakes_inert_gate() {
        let plugin = build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist");
        assert!(
            plugin.contains("const CAPTURE_MODE: \"allowlist\" | \"denylist\" = \"denylist\";"),
            "{plugin}"
        );
        assert!(plugin.contains(CAPTURE_ADMIT_GATE_TS), "{plugin}");
    }

    #[test]
    fn opencode_plugin_uses_bounded_hook_queue() {
        let plugin = build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist");

        assert_generated_ts_uses_bounded_hook_queue(&plugin);
    }

    #[test]
    fn opencode_plugin_resolves_token_at_runtime_when_not_embedded() {
        let plugin = build_opencode_plugin("http://127.0.0.1:49374", None, None, "denylist");
        assert!(plugin.contains("function resolveToken("));
        assert!(plugin.contains("const token = resolveToken();"));
        assert!(plugin.contains("if (!response.ok) return undefined;"));
    }

    #[test]
    fn curl_installer_accepts_generated_integration_agents() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("scripts")
            .join("install-hooks.sh");
        let Some(bash) = bash_program_for_installer_test() else {
            return;
        };

        for alias in ["opencode", "opencode2"] {
            let output = Command::new(&bash)
                .arg(&script)
                .arg("--agent")
                .arg(alias)
                .output()
                .unwrap_or_else(|e| {
                    panic!("failed to run {} for alias {alias}: {e}", script.display())
                });

            assert!(
                output.status.success(),
                "script rejected generated integration alias {alias}: stdout={}, stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );

            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.contains(&format!("install-hooks --agent {alias} --apply")));
        }
    }

    // ----------------------------------------------------------------
    // Claude Code / OpenCode / ZCode tests
    // ----------------------------------------------------------------

    #[test]
    fn claude_settings_path_honours_claude_config_dir() {
        let custom = if cfg!(windows) {
            r"C:\custom\claude"
        } else {
            "/custom/claude"
        };
        let path = claude_settings_path_in(Some(std::ffi::OsString::from(custom))).unwrap();
        assert_eq!(path, Path::new(custom).join("settings.json"));

        // Empty override and unset var both fall back to ~/.claude/settings.json.
        for env in [None, Some(std::ffi::OsString::new())] {
            let path = claude_settings_path_in(env).unwrap();
            assert!(
                path.ends_with(Path::new(".claude").join("settings.json")),
                "default must be ~/.claude/settings.json, got {}",
                path.display()
            );
        }
    }

    /// `CODEX_HOME` relocates Codex's entire config home, so hooks written to
    /// `~/.codex` are never loaded by a Codex configured that way — the install
    /// reports success and capture silently does nothing. ai-memory already
    /// honors the variable when resolving Codex transcripts, so the two halves
    /// of one install have to agree on where that home is.
    #[test]
    fn claude_code_apply_stages_into_injected_dir() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "session-end.sh",
                "user-prompt-submit.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "pre-compact.sh",
                "stop.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("settings.json");
        let staging_tmp = TempDir::new().unwrap();

        apply_to_claude_code_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            staging_tmp.path(),
            &InstallHooksArgs {
                profile: None,
                agent: AgentChoice::ClaudeCode,
                capture_assistant: false,
                no_capture_prompts: false,
                capture_mode: None,
                capture_prompts: false,
                hooks_dir: Some(hooks_tmp.path().to_path_buf()),
                server_url: Some("http://127.0.0.1:49374".to_string()),
                auth_token: None,
                config_file: Some(config_path.clone()),
                project_strategy: Some(ProjectStrategyArg::Basename),
                as_user: None,
                apply: false,
            },
        )
        .unwrap();

        let staged_script = staging_tmp
            .path()
            .join("hooks")
            .join("claude-code")
            .join("session-start.sh");
        assert!(
            staged_script.is_file(),
            "expected hook script staged at {}, override was not honoured",
            staged_script.display()
        );

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        let command = parsed
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(serde_json::Value::as_str)
            .expect("SessionStart command should be present");
        assert!(
            command.contains(&staged_script.to_string_lossy().into_owned()),
            "generated command must reference staged script {}: {command}",
            staged_script.display()
        );
    }

    #[test]
    fn claude_prompt_opt_out_survives_reapply_and_preserves_third_party_hook() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "session-end.sh",
                "user-prompt-submit.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "pre-compact.sh",
                "stop.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("settings.json");
        let staging_tmp = TempDir::new().unwrap();
        fs::write(
            &config_path,
            serde_json::json!({
                "hooks": {
                    "UserPromptSubmit": [
                        { "hooks": [{ "command": "third-party prompt guard" }] },
                        { "hooks": [{ "command": "/old/ai-memory hook --event user-prompt --agent claude-code --server-url http://old" }] }
                    ]
                }
            })
            .to_string(),
        )
        .unwrap();

        let disabled = InstallHooksArgs {
            agent: AgentChoice::ClaudeCode,
            config_file: Some(config_path.clone()),
            no_capture_prompts: true,
            capture_mode: None,
            ..default_hook_args()
        };
        apply_to_claude_code_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            staging_tmp.path(),
            &disabled,
        )
        .unwrap();

        let assert_disabled = || {
            let parsed: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
            let prompts = parsed["hooks"][CLAUDE_PROMPT_EVENT]
                .as_array()
                .expect("third-party prompt hook keeps the event present");
            assert_eq!(prompts.len(), 1);
            assert!(
                serde_json::to_string(prompts)
                    .unwrap()
                    .contains("third-party prompt guard")
            );
            assert!(!prompts.iter().any(is_ai_memory_hook_entry));
        };
        assert_disabled();

        let bare_reapply = InstallHooksArgs {
            agent: AgentChoice::ClaudeCode,
            config_file: Some(config_path.clone()),
            ..default_hook_args()
        };
        apply_to_claude_code_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            staging_tmp.path(),
            &bare_reapply,
        )
        .unwrap();
        assert_disabled();

        let enabled = InstallHooksArgs {
            agent: AgentChoice::ClaudeCode,
            config_file: Some(config_path.clone()),
            capture_prompts: true,
            ..default_hook_args()
        };
        apply_to_claude_code_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            staging_tmp.path(),
            &enabled,
        )
        .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        let prompts = parsed["hooks"][CLAUDE_PROMPT_EVENT].as_array().unwrap();
        assert_eq!(prompts.len(), 2, "third-party + one ai-memory hook");
        assert_eq!(
            prompts
                .iter()
                .filter(|entry| is_ai_memory_hook_entry(entry))
                .count(),
            1
        );
    }

    #[test]
    fn startup_handoff_scripts_forward_native_session_ids() {
        let hooks_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("hooks");
        for agent in ["claude-code", "opencode"] {
            let script = fs::read_to_string(hooks_root.join(agent).join("session-start.sh"))
                .unwrap()
                .replace("\r\n", "\n");
            assert!(
                script.contains("SESSION_ID=$(ai_memory_extract_session_id \"$PAYLOAD\")"),
                "{agent} must extract the native receiver session id"
            );
            assert!(
                script.contains("${SESSION_QS}"),
                "{agent} must forward the native receiver session id to /handoff"
            );
        }
    }
}
