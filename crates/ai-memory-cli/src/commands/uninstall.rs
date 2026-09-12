//! `ai-memory uninstall` — the symmetric inverse of install-hooks /
//! install-mcp / install-instructions. Detects ai-memory's wiring in
//! every supported agent's config and removes only that, never
//! third-party entries. Optional `--purge-data` wipes wiki/db/raw via
//! the reset path. Docker teardown is printed, never executed.
//!
//! Design: docs/superpowers/specs/2026-05-24-uninstall-command-design.md

use crate::cli::McpClient;
use crate::cli::UninstallArgs;
use crate::commands::apply_shared::apply_atomic;
use crate::commands::apply_shared::mutate_json;
use crate::commands::path_util::{claude_config_dir, claude_config_paths, home_dir};
use crate::commands::{data_purge, install_hooks, install_mcp};
use crate::config::Config;
use ai_memory_core::routing_skills::{
    AGENTS_SKILL_DIR, CLAUDE_SKILL_DIR, MANAGED_MARKER, MANAGED_SKILLS, SKILLS_DIR,
};
use ai_memory_core::{MARKER_END, MARKER_START, find_marker_line};
use anyhow::{Context, Result};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

const LEGACY_ORPHAN_TAIL_LF: &str =
    "` markers without\ndisturbing the rest of the file.\n<!-- ai-memory:end -->\n";
const LEGACY_ORPHAN_TAIL_CRLF: &str =
    "` markers without\r\ndisturbing the rest of the file.\r\n<!-- ai-memory:end -->\r\n";

/// One rewrite operation to apply to a config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RewriteOp {
    /// CLAUDE.md / AGENTS.md routing block.
    Instructions,
    /// Standard JSON hook table under `hooks`.
    HooksJson,
    /// MCP JSON config for one client shape.
    McpJson(McpClient),
}

/// Generated files that uninstall may delete after content re-validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteKind {
    OpenCodePlugin,
    OpenCode2Plugin,
    ManagedSkill,
}

impl DeleteKind {
    const fn label(self) -> &'static str {
        match self {
            Self::OpenCodePlugin => "OpenCode plugin",
            Self::OpenCode2Plugin => "OpenCode 2 plugin",
            Self::ManagedSkill => "managed Agent Skill",
        }
    }
}

/// One file the uninstall will touch, plus what it will do to it.
#[derive(Debug)]
enum PlannedChange {
    /// JSON/TOML rewrite removing the listed items (events or server names).
    Rewrite {
        path: PathBuf,
        removed: Vec<String>,
        ops: Vec<RewriteOp>,
    },
    /// Whole-file delete, limited to generated files whose contents still
    /// prove they are ai-memory-owned at apply time.
    DeleteFile { path: PathBuf, kind: DeleteKind },
}

fn push_rewrite(plan: &mut Vec<PlannedChange>, path: PathBuf, removed: Vec<String>, op: RewriteOp) {
    if removed.is_empty() {
        return;
    }
    for change in plan.iter_mut() {
        if let PlannedChange::Rewrite {
            path: existing,
            removed: existing_removed,
            ops,
        } = change
            && *existing == path
        {
            existing_removed.extend(removed);
            if !ops.contains(&op) {
                ops.push(op);
            }
            return;
        }
    }
    plan.push(PlannedChange::Rewrite {
        path,
        removed,
        ops: vec![op],
    });
}

fn push_generated_delete(plan: &mut Vec<PlannedChange>, path: PathBuf, kind: DeleteKind) {
    if generated_file_is_ours(&path, kind) {
        plan.push(PlannedChange::DeleteFile { path, kind });
    }
}

/// Build the full removal plan by reading each existing config file and
/// running the matching pure stripper. Missing files / no-matches
/// produce no entry. `name`/`url` identify the MCP server.
fn build_plan(args: &UninstallArgs) -> anyhow::Result<Vec<PlannedChange>> {
    let mut plan = Vec::new();
    let want = |k: crate::cli::UninstallOnly| args.only.is_none() || args.only == Some(k);
    let name = args.mcp_name.as_deref();
    let url = args.mcp_url.as_str();
    let home = home_dir();
    let claude_config_dir = claude_config_dir(std::env::var_os("CLAUDE_CONFIG_DIR"));

    // ---- Hooks (JSON configs) ----
    if want(crate::cli::UninstallOnly::Hooks) {
        let hook_files: Vec<_> = claude_config_paths(
            home.as_deref(),
            claude_config_dir.as_deref(),
            Path::new(".claude/settings.json"),
            Path::new("settings.json"),
        )
        .into_iter()
        .collect();
        for path in hook_files {
            if !path.exists() {
                continue;
            }
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let removal = strip_ai_memory_hooks(&content)?;
            push_rewrite(&mut plan, path, removal.removed_events, RewriteOp::HooksJson);
        }

        let plugin = install_hooks::opencode_plugin_path()?;
        push_generated_delete(&mut plan, plugin, DeleteKind::OpenCodePlugin);

        let plugin2 = install_hooks::opencode2_plugin_path()?;
        push_generated_delete(&mut plan, plugin2, DeleteKind::OpenCode2Plugin);
    }

    // ---- MCP (per client) ----
    if want(crate::cli::UninstallOnly::Mcp) {
        use crate::cli::McpClient::*;
        for client in [ClaudeCode, OpenCode, OpenCode2, Zcode] {
            let paths = if matches!(client, ClaudeCode) {
                claude_config_paths(
                    home.as_deref(),
                    claude_config_dir.as_deref(),
                    Path::new(".claude.json"),
                    Path::new(".claude.json"),
                )
            } else {
                let Ok(path) = install_mcp::mcp_config_path(client) else {
                    continue;
                };
                vec![path]
            };
            for path in paths {
                if !path.exists() {
                    continue;
                }
                let content = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let (_new, removed) = strip_mcp_json_client(&content, client, name, url)?;
                push_rewrite(&mut plan, path, removed, RewriteOp::McpJson(client));
            }
        }
    }

    // ---- Instructions (cwd CLAUDE.md / AGENTS.md) ----
    if want(crate::cli::UninstallOnly::Instructions) {
        let cwd = std::env::current_dir().context("getting CWD for instruction removal")?;
        for name_md in ["CLAUDE.md", "AGENTS.md"] {
            let path = cwd.join(name_md);
            if !path.exists() {
                continue;
            }
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let (_new, found) = strip_instructions_block(&content);
            if found {
                push_rewrite(
                    &mut plan,
                    path,
                    vec!["instruction block".to_string()],
                    RewriteOp::Instructions,
                );
            }
        }
    }

    // ---- Managed Agent Skills (project + global roots) ----
    if want(crate::cli::UninstallOnly::Skills) {
        let cwd = std::env::current_dir().context("getting CWD for skill removal")?;
        for root in skill_roots(&cwd, home.as_deref(), claude_config_dir.as_deref()) {
            for skill in MANAGED_SKILLS {
                push_generated_delete(
                    &mut plan,
                    root.join(skill.relative_path),
                    DeleteKind::ManagedSkill,
                );
            }
        }
    }

    Ok(plan)
}

fn skill_roots(cwd: &Path, home: Option<&Path>, claude_config_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(10);
    push_unique_skill_root(&mut roots, cwd.join(CLAUDE_SKILL_DIR).join(SKILLS_DIR));
    push_unique_skill_root(&mut roots, cwd.join(AGENTS_SKILL_DIR).join(SKILLS_DIR));
    if let Some(home) = home {
        push_unique_skill_root(&mut roots, home.join(CLAUDE_SKILL_DIR).join(SKILLS_DIR));
        push_unique_skill_root(&mut roots, home.join(AGENTS_SKILL_DIR).join(SKILLS_DIR));
    }
    // With CLAUDE_CONFIG_DIR set, global Claude Code skills install to
    // `$CLAUDE_CONFIG_DIR/skills`. Sweep it alongside ~/.claude/skills —
    // installs may predate the env var.
    if let Some(claude_config_dir) = claude_config_dir {
        push_unique_skill_root(&mut roots, claude_config_dir.join(SKILLS_DIR));
    }
    roots
}

fn push_unique_skill_root(roots: &mut Vec<PathBuf>, root: PathBuf) {
    if !roots.iter().any(|existing| existing == &root) {
        roots.push(root);
    }
}

/// Print the plan, one line per file, mirroring `reset`'s dry-run style.
fn print_plan(plan: &[PlannedChange]) {
    if plan.is_empty() {
        println!("Nothing to remove. ai-memory wiring not found.");
        return;
    }
    for change in plan {
        match change {
            PlannedChange::Rewrite { path, removed, .. } => {
                println!(
                    "would remove {} from {}",
                    removed.join(", "),
                    path.display()
                );
            }
            PlannedChange::DeleteFile { path, kind } => {
                println!("would delete {} ({})", path.display(), kind.label());
            }
        }
    }
}

/// Re-run the planned strippers inside `apply_atomic` so the actual write is
/// atomic + backed up. Planning records exact operations per file, so shared
/// files such as `~/.gemini/settings.json` only apply the selected concerns.
fn apply_change(change: &PlannedChange, name: Option<&str>, url: &str) -> anyhow::Result<()> {
    match change {
        PlannedChange::DeleteFile { path, kind } => {
            if !path.exists() {
                return Ok(());
            }
            if !generated_file_is_ours(path, *kind) {
                println!(
                    "skipped {} because it no longer looks like an ai-memory-generated {}",
                    path.display(),
                    kind.label()
                );
                return Ok(());
            }
            std::fs::remove_file(path).with_context(|| format!("deleting {}", path.display()))?;
            println!("✓ deleted {}", path.display());
            if *kind == DeleteKind::ManagedSkill {
                remove_empty_skill_dirs(path)?;
            }
        }
        PlannedChange::Rewrite { path, ops, .. } => {
            let outcome = apply_atomic(path, |existing| {
                let mut out = existing.to_string();
                for op in ops {
                    out = match *op {
                        RewriteOp::Instructions => strip_instructions_block(&out).0,
                        RewriteOp::HooksJson => strip_ai_memory_hooks(&out)?.new_content,
                        RewriteOp::McpJson(client) => {
                            strip_mcp_json_client(&out, client, name, url)?.0
                        }
                    };
                }
                Ok(out)
            })?;
            println!("✓ {} {}", outcome.verb(), path.display());
        }
    }
    Ok(())
}

fn remove_empty_skill_dirs(skill_file: &Path) -> Result<()> {
    let Some(skill_dir) = skill_file.parent() else {
        return Ok(());
    };
    let root = skill_dir.parent().map(Path::to_path_buf);

    remove_dir_if_empty(skill_dir)?;
    if let Some(root) = root {
        remove_dir_if_empty(&root)?;
    }

    Ok(())
}

fn remove_dir_if_empty(path: &Path) -> Result<()> {
    if !path.is_dir() {
        return Ok(());
    }

    let mut entries =
        std::fs::read_dir(path).with_context(|| format!("reading {}", path.display()))?;
    if entries.next().is_some() {
        return Ok(());
    }

    std::fs::remove_dir(path).with_context(|| format!("removing {}", path.display()))?;
    println!("✓ removed empty directory {}", path.display());
    Ok(())
}

/// Run the `uninstall` subcommand.
///
/// # Errors
/// Returns an error if a config file is malformed or a removal write
/// fails. Absent files / nothing-to-remove are not errors.
pub fn run(config: &Config, args: UninstallArgs) -> anyhow::Result<()> {
    let name = args.mcp_name.clone();
    let url = args.mcp_url.clone();

    let plan = build_plan(&args)?;
    print_plan(&plan);
    if args.purge_data {
        for path in data_purge::purge_preview(&config.data_dir) {
            println!("would purge {}", path.display());
        }
    }
    if !args.apply {
        println!("(dry-run; pass --apply to remove)");
        return Ok(());
    }
    if plan.is_empty() && !args.purge_data {
        return Ok(());
    }

    // All-or-nothing: when we're going to purge data, refuse before touching
    // anything if an ai-memory process is alive (matches reset's guard-at-top).
    // Wiring-only uninstall stays unguarded — it edits agent config files the
    // server never touches.
    if args.purge_data {
        let siblings = crate::process_guard::sibling_processes();
        if !siblings.is_empty() {
            anyhow::bail!(crate::process_guard::busy_message("purge data", &siblings));
        }
    }

    if std::io::stdin().is_terminal() && !args.yes {
        eprint!("Proceed with removal? [y/N] ");
        use std::io::Write as _;
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("aborted.");
            return Ok(());
        }
    }

    for change in &plan {
        apply_change(change, name.as_deref(), &url)?;
    }

    // Removing the hooks removes the only readers of the stored bearer, so
    // leaving it on disk would strand a live credential (#552). Best-effort:
    // an unremovable file must not fail a teardown that otherwise succeeded.
    if let Err(error) = crate::config::clear_hook_auth_token(&config.data_dir) {
        eprintln!(
            "ai-memory uninstall warning: could not remove the stored auth token under {}: {error}",
            config.data_dir.display()
        );
    }

    if args.purge_data {
        for path in data_purge::purge_data_dirs(&config.data_dir)? {
            println!("✓ purged {}", path.display());
        }
    }

    print_docker_hint(args.purge_data);

    Ok(())
}

/// Print the manual Docker teardown steps (never executed). When the
/// data was purged locally, note that; otherwise remind how to wipe it.
fn print_docker_hint(data_purged: bool) {
    println!();
    println!("Wiring removed. ai-memory's server + data live in its container/volume —");
    println!("tear those down manually:");
    println!("  docker compose -f docker/docker-compose.yml down -v");
    println!("  docker volume rm ai-memory-data   # if you used the default volume");
    println!("  rm -f bin/ai-memory               # the wrapper script, if installed");
    if !data_purged {
        println!();
        println!(
            "Local data dir was left intact. To wipe it: `ai-memory reset --confirm` (or re-run with --purge-data)."
        );
    }
}

/// Remove the `<!-- ai-memory:start -->`…`<!-- ai-memory:end -->`
/// block (inclusive) from a CLAUDE.md / AGENTS.md. Returns the new
/// content and whether a block was found. Inverse of
/// `install_instructions::merge_instructions_block`: an install
/// followed by an uninstall round-trips to the original file.
fn strip_instructions_block(content: &str) -> (String, bool) {
    // Line-anchored so an inline mention of the marker strings inside the
    // managed block can't be matched as the real end delimiter (which would
    // leave an orphan tail behind, breaking the install->uninstall
    // round-trip).
    let Some(start) = find_marker_line(content, MARKER_START, 0) else {
        return (content.to_string(), false);
    };
    let Some(end_pos) = find_marker_line(content, MARKER_END, start) else {
        return (content.to_string(), false);
    };
    let end = end_pos + MARKER_END.len();
    // Consume a trailing newline after the end marker if present.
    let after = if content.as_bytes().get(end..end + 2) == Some(b"\r\n") {
        end + 2
    } else if content.as_bytes().get(end).copied() == Some(b'\n') {
        end + 1
    } else {
        end
    };
    let mut head = content[..start].to_string();
    let tail = strip_legacy_orphan_tail(&content[after..]);
    // When the block sat at EOF, install added a blank-line separator
    // before it; drop that artifact so install→uninstall round-trips.
    if tail.is_empty() && head.ends_with("\n\n") {
        head.pop();
    }
    (format!("{head}{tail}"), true)
}

fn strip_legacy_orphan_tail(tail: &str) -> &str {
    let mut rest = tail;
    loop {
        if let Some(stripped) = rest.strip_prefix(LEGACY_ORPHAN_TAIL_LF) {
            rest = stripped;
        } else if let Some(stripped) = rest.strip_prefix(LEGACY_ORPHAN_TAIL_CRLF) {
            rest = stripped;
        } else {
            return rest;
        }
    }
}

/// True when a hook command string was written by ai-memory. Legacy script
/// commands carry the unconditional `AI_MEMORY_HOOK_URL=` env prefix; native
/// commands invoke the `ai-memory hook --event ... --server-url ...` subcommand.
/// Keep both signatures narrow so hook overlays and uninstall do not remove
/// unrelated hooks that happen to use the same event names or script basenames.
pub(crate) fn hook_command_is_ours(command: &str) -> bool {
    if command.contains("AI_MEMORY_HOOK_URL=") {
        return true;
    }
    let lower = command.to_ascii_lowercase();
    lower.contains("ai-memory")
        && lower.contains(" hook --event ")
        && lower.contains(" --agent ")
        && lower.contains(" --server-url ")
}

fn hook_entry_is_ours(entry: &serde_json::Value) -> bool {
    let Some(command) = entry.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    if hook_command_is_ours(command) {
        return true;
    }
    let lower = command.to_ascii_lowercase();
    if !(lower.contains("ai-memory") || lower.contains("ai_memory")) {
        return false;
    }
    let Some(args) = entry.get("args").and_then(|a| a.as_array()) else {
        return false;
    };
    let tokens: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
    tokens.contains(&"hook")
        && tokens.contains(&"--event")
        && tokens.contains(&"--agent")
        && tokens.contains(&"--server-url")
}

/// Result of stripping ai-memory entries from a hooks JSON file.
struct HookRemoval {
    new_content: String,
    removed_events: Vec<String>,
}

/// Remove ai-memory commands from one hook entry. Returns `(removed_any,
/// remove_entry)`. Flat entries are removed whole; nested entries only lose the
/// matching inner commands and survive when third-party inner hooks remain.
fn strip_hook_entry(entry: &mut serde_json::Value) -> (bool, bool) {
    if hook_entry_is_ours(entry) {
        return (true, true);
    }
    if let Some(inner) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) {
        let before = inner.len();
        inner.retain(|h| !hook_entry_is_ours(h));
        let removed = inner.len() != before;
        return (removed, inner.is_empty());
    }
    (false, false)
}

fn strip_hook_events(
    hooks: &mut serde_json::Map<String, serde_json::Value>,
    removed_events: &mut Vec<String>,
) {
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(arr) = hooks.get_mut(&event).and_then(|v| v.as_array_mut()) else {
            continue;
        };
        let mut removed_from_event = false;
        arr.retain_mut(|entry| {
            let (removed, remove_entry) = strip_hook_entry(entry);
            removed_from_event |= removed;
            !remove_entry
        });
        if removed_from_event {
            removed_events.push(event.clone());
        }
        if arr.is_empty() {
            hooks.remove(&event);
        }
    }
}

/// Remove ai-memory hook entries from a settings/hooks JSON document.
/// Preserves third-party entries (including siblings under the same
/// event). Prunes an event key when emptied and the `hooks` object
/// when emptied. Detection is by signature, so stale event keys
/// outside the current vocabulary are caught too.
fn strip_ai_memory_hooks(content: &str) -> Result<HookRemoval> {
    let mut removed_events = Vec::new();
    let new_content = mutate_json(content, |root| {
        let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
            return Ok(());
        };
        strip_hook_events(hooks, &mut removed_events);
        if hooks.is_empty() {
            root.remove("hooks");
        }
        Ok(())
    })?;
    Ok(HookRemoval {
        new_content,
        removed_events,
    })
}

fn generated_file_is_ours(path: &Path, kind: DeleteKind) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    match kind {
        DeleteKind::OpenCodePlugin => {
            content.contains("Auto-generated by `ai-memory install-hooks --agent opencode --apply`")
                && content.contains("const AGENT = \"open-code\";")
        }
        DeleteKind::OpenCode2Plugin => {
            content
                .contains("Auto-generated by `ai-memory install-hooks --agent opencode2 --apply`")
                && content.contains("const AGENT = \"opencode2\";")
        }
        DeleteKind::ManagedSkill => content.contains(MANAGED_MARKER),
    }
}

/// Where the servers object lives in each JSON client's config.
fn mcp_servers_path(client: McpClient) -> Option<&'static [&'static str]> {
    match client {
        McpClient::ClaudeCode => Some(&["mcpServers"]),
        McpClient::OpenCode => Some(&["mcp"]),
        McpClient::OpenCode2 => Some(&["mcp", "servers"]),
        McpClient::Zcode => Some(&["mcp", "servers"]),
    }
}

/// True when an MCP server entry is ai-memory's: its url/httpUrl/serverUrl
/// equals the endpoint, or it is an ai-memory-owned / `mcp-remote` stdio
/// bridge whose args contain the endpoint. The key/name alone is intentionally
/// not enough: users may have unrelated entries named `ai-memory`, and
/// uninstall must not remove them unless the endpoint also matches.
fn mcp_entry_is_ours(key: &str, entry: &serde_json::Value, name: Option<&str>, url: &str) -> bool {
    if name.is_some_and(|name| key != name) {
        return false;
    }
    for field in ["url", "httpUrl", "serverUrl"] {
        if entry.get(field).and_then(|v| v.as_str()) == Some(url) {
            return true;
        }
    }
    if let Some(args) = entry.get("args").and_then(|a| a.as_array()) {
        let has_remote = args.iter().any(|a| a.as_str() == Some("mcp-remote"));
        let has_session_bridge = entry.get("command").and_then(|v| v.as_str()) == Some("ai-memory")
            && args.iter().any(|a| a.as_str() == Some("mcp-bridge"));
        let has_url = args.iter().any(|a| a.as_str() == Some(url));
        if (has_remote || has_session_bridge) && has_url {
            return true;
        }
    }
    false
}

/// URL forms uninstall matches for `client`: currently just the endpoint
/// as given — no kept client rewrites the URL when installing.
fn mcp_url_candidates(_client: McpClient, url: &str) -> Vec<String> {
    vec![url.to_string()]
}

/// [`strip_mcp_json`] over each URL candidate form of the client.
fn strip_mcp_json_client(
    content: &str,
    client: McpClient,
    name: Option<&str>,
    url: &str,
) -> Result<(String, Vec<String>)> {
    let mut new_content = content.to_string();
    let mut removed = Vec::new();
    for candidate in mcp_url_candidates(client, url) {
        let (next, mut hits) = strip_mcp_json(&new_content, client, name, &candidate)?;
        new_content = next;
        removed.append(&mut hits);
    }
    Ok((new_content, removed))
}

/// Remove ai-memory's MCP server from a JSON client config. Returns
/// the new content and the names removed. Prunes the (possibly nested)
/// servers object and its parents if they empty.
fn strip_mcp_json(
    content: &str,
    client: McpClient,
    name: Option<&str>,
    url: &str,
) -> Result<(String, Vec<String>)> {
    let Some(path) = mcp_servers_path(client) else {
        return Ok((content.to_string(), Vec::new()));
    };
    let mut removed = Vec::new();
    let new_content = mutate_json(content, |root| {
        let mut cursor: &mut serde_json::Map<String, serde_json::Value> = root;
        for (depth, key) in path.iter().enumerate() {
            let is_last = depth == path.len() - 1;
            if is_last {
                let Some(servers) = cursor.get_mut(*key).and_then(|v| v.as_object_mut()) else {
                    return Ok(());
                };
                let keys: Vec<String> = servers.keys().cloned().collect();
                for k in keys {
                    let ours = servers
                        .get(&k)
                        .is_some_and(|e| mcp_entry_is_ours(&k, e, name, url));
                    if ours {
                        servers.remove(&k);
                        removed.push(k);
                    }
                }
                if servers.is_empty() {
                    cursor.remove(*key);
                }
            } else {
                let Some(next) = cursor.get_mut(*key).and_then(|v| v.as_object_mut()) else {
                    return Ok(());
                };
                cursor = next;
            }
        }
        Ok(())
    })?;
    Ok((new_content, removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_roots_sweep_claude_config_dir_root_alongside_home() {
        let roots = skill_roots(
            Path::new("/repo"),
            Some(Path::new("/home/alice")),
            Some(Path::new("/stores/claude")),
        );
        assert!(
            roots.contains(&PathBuf::from("/stores/claude/skills")),
            "{roots:?}"
        );
        assert!(
            roots.contains(&PathBuf::from("/home/alice/.claude/skills")),
            "{roots:?}"
        );
    }

    #[test]
    fn strip_instructions_round_trips_with_install_append() {
        let original = "# Title\n";
        // Mirror install_instructions::merge append behavior:
        let block = format!("{MARKER_START}\nBODY\n{MARKER_END}\n");
        let installed = format!("{original}\n{block}");
        let (stripped, found) = strip_instructions_block(&installed);
        assert!(found);
        assert_eq!(
            stripped, original,
            "uninstall must restore the original file"
        );
    }

    /// Regression: a managed block whose body mentions the end marker
    /// inline must be stripped up to the REAL delimiter, not the inline
    /// mention — otherwise an orphan tail survives the uninstall.
    #[test]
    fn strip_ignores_inline_marker_mention() {
        let original = "# Title\n";
        let block =
            format!("{MARKER_START}\nsee the `{MARKER_END}` marker inline\nbody\n{MARKER_END}\n");
        let installed = format!("{original}\n{block}");
        let (stripped, found) = strip_instructions_block(&installed);
        assert!(found);
        assert_eq!(
            stripped, original,
            "no orphan tail after the real end marker"
        );
    }

    #[test]
    fn strip_consumes_crlf_after_end_marker() {
        let content = format!("# Top\r\n\r\n{MARKER_START}\r\nBODY\r\n{MARKER_END}\r\nMore\r\n");
        let (stripped, found) = strip_instructions_block(&content);
        assert!(found);
        assert_eq!(stripped, "# Top\r\n\r\nMore\r\n");
    }

    #[test]
    fn strip_removes_exact_legacy_orphan_tail() {
        let content =
            format!("# Top\n\n{MARKER_START}\nBODY\n{MARKER_END}\n{LEGACY_ORPHAN_TAIL_LF}More\n");
        let (stripped, found) = strip_instructions_block(&content);
        assert!(found);
        assert_eq!(stripped, "# Top\n\nMore\n");
    }

    #[test]
    fn strip_removes_repeated_legacy_orphan_tails() {
        let content = format!(
            "# Top\n\n{MARKER_START}\nBODY\n{MARKER_END}\n{LEGACY_ORPHAN_TAIL_LF}{LEGACY_ORPHAN_TAIL_CRLF}More\n"
        );
        let (stripped, found) = strip_instructions_block(&content);
        assert!(found);
        assert_eq!(stripped, "# Top\n\nMore\n");
    }

    #[test]
    fn strip_instructions_preserves_surrounding_content() {
        let content = format!("# Top\n\n{MARKER_START}\nBODY\n{MARKER_END}\n\nMore notes.\n");
        let (stripped, found) = strip_instructions_block(&content);
        assert!(found);
        assert!(stripped.contains("# Top"));
        assert!(stripped.contains("More notes."));
        assert!(!stripped.contains("BODY"));
        assert!(!stripped.contains(MARKER_START));
    }

    #[test]
    fn strip_instructions_no_block_is_noop() {
        let content = "# Just a readme\n";
        let (stripped, found) = strip_instructions_block(content);
        assert!(!found);
        assert_eq!(stripped, content);
    }

    #[test]
    fn hook_signature_matches_no_auth_default() {
        let cmd = "AI_MEMORY_HOOK_URL=http://127.0.0.1:49374 /home/u/.local/share/ai-memory/hooks/claude-code/stop.sh";
        assert!(hook_command_is_ours(cmd));
    }

    #[test]
    fn hook_signature_matches_with_auth_and_custom_prefix() {
        let cmd = "AI_MEMORY_HOOK_URL=http://lan:49374 AI_MEMORY_AUTH_TOKEN=abc /etc/custom/session-start.sh";
        assert!(hook_command_is_ours(cmd));
    }

    #[test]
    fn hook_signature_matches_native_posix_command() {
        let cmd = "'/home/alice/.cargo/bin/ai-memory' --data-dir '/tmp/custom data' hook --event session-start --agent claude-code --server-url http://h:49374";
        assert!(hook_command_is_ours(cmd));
    }

    #[test]
    fn hook_signature_matches_native_windows_command() {
        let cmd = r#""C:\Users\alice\bin\ai-memory.exe" --data-dir "C:\Users\alice\AppData\Local\ai-memory" hook --event session-start --agent claude-code --server-url "http://h:49374""#;
        assert!(hook_command_is_ours(cmd));
    }

    /// #515 gave Codex's Windows command a leading `& ` call operator.
    /// `hook_command_is_ours` matches on substrings, so the prefix is
    /// harmless — but if that ever became a prefix match, uninstall would
    /// silently stop finding Codex hooks and leave them behind.
    #[test]
    fn hook_signature_matches_a_powershell_call_operator_command() {
        let cmd = r#"& "C:\Users\alice\bin\ai-memory.exe" --data-dir "C:\Users\alice\AppData\Local\ai-memory" hook --event session-start --agent codex --server-url "http://h:49374""#;
        assert!(hook_command_is_ours(cmd));
    }

    #[test]
    fn hook_signature_rejects_third_party_with_generic_name() {
        // A user's own hook that happens to be named stop.sh — no prefix.
        assert!(!hook_command_is_ours("/usr/local/bin/my-stop.sh"));
        assert!(!hook_command_is_ours("/opt/tools/hooks/session-start.sh"));
        assert!(!hook_command_is_ours(
            "/usr/local/bin/something hook --event stop --agent claude-code --server-url http://h"
        ));
    }

    #[test]
    fn strip_hooks_nested_removes_ours_keeps_third_party() {
        let content = r#"{
      "hooks": {
        "SessionStart": [
          {"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=http://h /x/session-start.sh"}]}
        ],
        "Notification": [
          {"matcher":"","hooks":[{"type":"command","command":"/usr/bin/notify.sh"}]}
        ]
      }
    }"#;
        let out = strip_ai_memory_hooks(content).unwrap();
        assert_eq!(out.removed_events, vec!["SessionStart".to_string()]);
        let v: serde_json::Value = serde_json::from_str(&out.new_content).unwrap();
        assert!(v["hooks"].get("SessionStart").is_none(), "our event pruned");
        assert!(v["hooks"].get("Notification").is_some(), "third-party kept");
    }

    #[test]
    fn strip_hooks_prunes_emptied_hooks_object() {
        let content = r#"{"hooks":{"Stop":[{"type":"command","command":"AI_MEMORY_HOOK_URL=x /a/stop.sh"}]}}"#;
        let out = strip_ai_memory_hooks(content).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out.new_content).unwrap();
        assert!(v.get("hooks").is_none(), "emptied hooks object removed");
    }

    #[test]
    fn strip_hooks_preserves_third_party_with_generic_basename() {
        let content = r#"{
      "hooks": {
        "Stop": [
          {"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=x /a/stop.sh"}]},
          {"matcher":"","hooks":[{"type":"command","command":"/home/u/scripts/stop.sh"}]}
        ]
      }
    }"#;
        let out = strip_ai_memory_hooks(content).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out.new_content).unwrap();
        let arr = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "only ours removed");
        assert!(
            arr[0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("/home/u/scripts/stop.sh")
        );
    }

    #[test]
    fn strip_hooks_nested_mixed_inner_commands_preserves_user_hook() {
        let content = r#"{
      "hooks": {
        "Stop": [
          {"matcher":"","hooks":[
            {"type":"command","command":"AI_MEMORY_HOOK_URL=x /a/stop.sh"},
            {"type":"command","command":"/home/u/scripts/my-stop.sh"}
          ]}
        ]
      }
    }"#;

        let out = strip_ai_memory_hooks(content).unwrap();
        assert_eq!(out.removed_events, vec!["Stop".to_string()]);
        let v: serde_json::Value = serde_json::from_str(&out.new_content).unwrap();
        let inner = v["hooks"]["Stop"][0]["hooks"].as_array().unwrap();
        assert_eq!(inner.len(), 1, "third-party inner hook must survive");
        assert_eq!(
            inner[0]["command"].as_str(),
            Some("/home/u/scripts/my-stop.sh")
        );
    }

    #[test]
    fn strip_hooks_removes_exec_form_ours_preserves_exec_third_party_and_sibling() {
        let content = r#"{
      "hooks": {
        "SessionStart": [
          {"matcher":"","hooks":[
            {"type":"command","command":"C:\\bin\\ai-memory.exe","args":["hook","--event","session-start","--agent","claude-code","--server-url","http://h"]},
            {"type":"command","command":"C:\\bin\\third-party.exe","args":["hook","--event","session-start","--agent","claude-code","--server-url","http://h"]}
          ]},
          {"matcher":"Tool","hooks":[
            {"type":"command","command":"C:\\bin\\other.exe","args":["--keep"]}
          ]}
        ],
        "Stop": [
          {"matcher":"","hooks":[{"type":"command","command":"\"C:\\bin\\ai-memory.exe\" hook --event stop --agent claude-code --server-url \"http://h\""}]}
        ]
      }
    }"#;

        let out = strip_ai_memory_hooks(content).unwrap();
        assert_eq!(
            out.removed_events,
            vec!["SessionStart".to_string(), "Stop".to_string()]
        );
        let v: serde_json::Value = serde_json::from_str(&out.new_content).unwrap();
        assert!(
            v["hooks"].get("Stop").is_none(),
            "legacy string hook removed"
        );
        let entries = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "outer sibling group preserved");
        let first_inner = entries[0]["hooks"].as_array().unwrap();
        assert_eq!(
            first_inner.len(),
            1,
            "only ai-memory inner exec hook removed"
        );
        assert_eq!(
            first_inner[0]["command"].as_str(),
            Some(r"C:\bin\third-party.exe")
        );
        assert_eq!(
            entries[1]["hooks"][0]["command"].as_str(),
            Some(r"C:\bin\other.exe")
        );
    }

    #[test]
    fn strip_hooks_no_hooks_key_is_noop() {
        let content = r#"{"unrelated":true}"#;
        let out = strip_ai_memory_hooks(content).unwrap();
        assert!(out.removed_events.is_empty());
    }

    // Issue #156: uninstall removes only ai-memory's entries from Zero's
    // hooks.json, keyed by the id prefix, and leaves everything else alone.
    #[test]
    fn generated_file_detection_rejects_user_files_at_ours_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ai-memory.ts");
        std::fs::write(&path, "// my personal plugin named ai-memory\n").unwrap();

        assert!(!generated_file_is_ours(&path, DeleteKind::OpenCodePlugin));

        std::fs::write(
            &path,
            "// Auto-generated by `ai-memory install-hooks --agent opencode --apply`.\nconst AGENT = \"open-code\";\n",
        )
        .unwrap();
        assert!(generated_file_is_ours(&path, DeleteKind::OpenCodePlugin));
    }

    #[test]
    fn strip_mcp_claude_by_name_keeps_others() {
        let content = r#"{"mcpServers":{"ai-memory":{"type":"http","url":"http://127.0.0.1:49374/mcp"},"other":{"url":"http://x"}}}"#;
        let (out, removed) = strip_mcp_json(
            content,
            McpClient::ClaudeCode,
            Some("ai-memory"),
            "http://127.0.0.1:49374/mcp",
        )
        .unwrap();
        assert_eq!(removed, vec!["ai-memory".to_string()]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["mcpServers"].get("ai-memory").is_none());
        assert!(v["mcpServers"].get("other").is_some());
    }

    #[test]
    fn strip_mcp_by_endpoint_under_custom_name() {
        let content = r#"{"mcpServers":{"my-mem":{"url":"http://127.0.0.1:49374/mcp"}}}"#;
        let (out, removed) = strip_mcp_json(
            content,
            McpClient::ClaudeCode,
            None,
            "http://127.0.0.1:49374/mcp",
        )
        .unwrap();
        assert_eq!(
            removed,
            vec!["my-mem".to_string()],
            "matched by endpoint, not name"
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(
            v.get("mcpServers").is_none(),
            "emptied servers object pruned"
        );
    }

    #[test]
    fn strip_mcp_name_only_does_not_remove_user_entry() {
        let content = r#"{"mcpServers":{"ai-memory":{"url":"http://example.invalid/mcp"}}}"#;
        let (out, removed) = strip_mcp_json(
            content,
            McpClient::ClaudeCode,
            Some("ai-memory"),
            "http://127.0.0.1:49374/mcp",
        )
        .unwrap();

        assert!(removed.is_empty());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["mcpServers"].get("ai-memory").is_some());
    }

    #[test]
    fn strip_mcp_no_match_is_noop() {
        let content = r#"{"mcpServers":{"other":{"url":"http://x"}}}"#;
        let (_out, removed) = strip_mcp_json(
            content,
            McpClient::ClaudeCode,
            Some("ai-memory"),
            "http://127.0.0.1:49374/mcp",
        )
        .unwrap();
        assert!(removed.is_empty());
    }

}
