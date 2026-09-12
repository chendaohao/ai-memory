//! `ai-memory install-mcp` — print the MCP server registration
//! snippet for any supported client.
//!
//! The snippet format and the config-file location differ across
//! clients. We render the *content* the user needs to paste; we
//! deliberately do not auto-edit their config (formats are evolving
//! upstream and a bad merge is very user-visible).
//!
//! OMP uses a native `~/.omp/agent/mcp.json` file with the same
//! `mcpServers` root as several other clients.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::cli::{InstallMcpArgs, McpClient};
use crate::commands::apply_shared::{ApplyOutcome, apply_atomic, mutate_json};
use crate::commands::path_util::{claude_config_dir, home_dir};
use crate::commands::render_shared::bearer_header_value;
use crate::config::{Config, DEFAULT_MCP_URL};


#[derive(Clone, Copy)]
enum JsonMcpLocation {
    RootMcpServers,
    RootMcp,
    NestedMcpServers,
}

/// Run the `install-mcp` subcommand.
///
/// # Errors
/// Returns an error if JSON serialisation fails (should never happen
/// for our handcrafted values).
pub fn run(config: &Config, args: InstallMcpArgs) -> Result<()> {
    let server_url = effective_mcp_server_url(config, &args);
    let args = InstallMcpArgs {
        server_url: Some(server_url),
        auth_token: args.auth_token.or_else(|| config.auth.bearer_token.clone()),
        ..args
    };
    validate_args(&args)?;
    if args.apply {
        return apply_to_config_file(&args);
    }
    let snippet = match args.client {
        McpClient::ClaudeCode => render_claude_code(&args, &resolve_config_file(&args)?)?,
        McpClient::OpenCode => render_opencode(&args)?,
        McpClient::OpenCode2 => render_opencode2(&args)?,
        McpClient::Zcode => render_zcode(&args)?,
    };
    println!("{snippet}");
    Ok(())
}

fn effective_mcp_server_url(config: &Config, args: &InstallMcpArgs) -> String {
    if let Some(url) = &args.server_url {
        // Normalize an explicit --server-url exactly like the config/env
        // branch below: users habitually pass the BASE url (the same value
        // `install-hooks --server-url` takes), and returning it verbatim
        // rendered a config pointing at the server root, which 404s (#185).
        // `mcp_server_url_from_base` is idempotent for full `/mcp` endpoints,
        // so callers who already pass the endpoint are unchanged.
        return mcp_server_url_from_base(url);
    }
    if config.server_url_configured() {
        return mcp_server_url_from_base(&config.server_url);
    }
    DEFAULT_MCP_URL.to_string()
}

pub(crate) fn mcp_server_url_from_base(server_url: &str) -> String {
    let trimmed = server_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/mcp") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/mcp")
    }
}

fn validate_args(args: &InstallMcpArgs) -> Result<()> {
    if args.session_aware && !matches!(args.client, McpClient::ClaudeCode) {
        bail!("--session-aware is supported only for --client claude-code");
    }
    Ok(())
}

/// Default MCP config-file path for a client (ignores any
/// `--config-file` override). Shared by install and uninstall.
///
/// # Errors
/// Returns an error when `$HOME` can't be resolved.
pub(crate) fn mcp_config_path(client: crate::cli::McpClient) -> Result<PathBuf> {
    use crate::cli::McpClient;
    let home = || home_dir().context("could not locate $HOME for config-file auto-detect");
    Ok(match client {
        McpClient::ClaudeCode => claude_code_config_path_in(std::env::var_os("CLAUDE_CONFIG_DIR"))?,
        McpClient::OpenCode => home()?
            .join(".config")
            .join("opencode")
            .join("opencode.json"),
        // V2 reads the same global config file (`opencode.json(c)`); its
        // `mcp.servers` key coexists with v1's `mcp` key in one strict-JSON
        // file, so both binaries stay wired side by side. Users who keep
        // comments in `opencode.jsonc` should pass it via `--config-file`
        // (`mutate_json` refuses to rewrite non-strict JSON).
        McpClient::OpenCode2 => home()?
            .join(".config")
            .join("opencode")
            .join("opencode.json"),
        // ZCode keeps its user-scope config at ~/.zcode/cli/config.json
        // (workspace scopes like .zcode/config.json exist, but user scope
        // is the install default for every other client too).
        McpClient::Zcode => home()?.join(".zcode").join("cli").join("config.json"),
    })
}

/// Claude Code reads MCP-server registrations from `.claude.json`
/// (the same file `claude mcp add`/`claude mcp list` operate on) —
/// `$CLAUDE_CONFIG_DIR/.claude.json` when the var is set, else
/// `~/.claude.json`. `settings.json` is a separate file for hooks /
/// permissions / etc. — putting `mcpServers` there does NOT make
/// Claude Code load the server. (Confirmed against CC 1.x by
/// observing that `mcpServers` in settings.json is silently ignored
/// while the same entry under `~/.claude.json` shows up in
/// `claude mcp list`.) The env value comes in as a parameter so tests
/// can exercise both branches without mutating process env.
fn claude_code_config_path_in(env_override: Option<std::ffi::OsString>) -> Result<PathBuf> {
    if let Some(dir) = claude_config_dir(env_override) {
        return Ok(dir.join(".claude.json"));
    }
    Ok(home_dir()
        .context("could not locate $HOME for ~/.claude.json")?
        .join(".claude.json"))
}


/// Resolve the user-config file for this client. Honours
/// `--config-file` when provided, else uses the canonical default
/// per client.
fn resolve_config_file(args: &InstallMcpArgs) -> Result<PathBuf> {
    if let Some(p) = &args.config_file {
        return Ok(p.clone());
    }
    mcp_config_path(args.client)
}

/// Mutate the resolved client config file in place. Idempotent —
/// re-runs that produce the same content are reported as no-op.
fn apply_to_config_file(args: &InstallMcpArgs) -> Result<()> {
    let path = resolve_config_file(args)?;
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| upsert_json_mcp_entry(root, args))
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

fn json_mcp_location(client: McpClient) -> Option<JsonMcpLocation> {
    match client {
        McpClient::ClaudeCode => Some(JsonMcpLocation::RootMcpServers),
        McpClient::OpenCode => Some(JsonMcpLocation::RootMcp),
        McpClient::OpenCode2 | McpClient::Zcode => Some(JsonMcpLocation::NestedMcpServers),
    }
}

fn build_json_mcp_entry(args: &InstallMcpArgs) -> Result<serde_json::Value> {
    validate_args(args)?;
    match args.client {
        McpClient::OpenCode => build_mcp_entry_opencode(args),
        McpClient::OpenCode2 => build_mcp_entry_opencode2(args),
        McpClient::Zcode => build_mcp_entry_zcode(args),
        _ => build_mcp_entry(args),
    }
}

fn upsert_json_mcp_entry(
    root: &mut serde_json::Map<String, serde_json::Value>,
    args: &InstallMcpArgs,
) -> Result<()> {
    let entry = build_json_mcp_entry(args)?;
    match json_mcp_location(args.client).context("internal: unsupported JSON MCP client")? {
        JsonMcpLocation::RootMcpServers => {
            let servers = root
                .entry("mcpServers")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`mcpServers` is present but not an object")?;
            servers.insert(args.name.clone(), entry);
        }
        JsonMcpLocation::RootMcp => {
            let mcp = root
                .entry("mcp")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`mcp` is present but not an object")?;
            mcp.insert(args.name.clone(), entry);
        }
        JsonMcpLocation::NestedMcpServers => {
            let mcp = root
                .entry("mcp")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`mcp` is present but not an object")?;
            let servers = mcp
                .entry("servers")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`mcp.servers` is present but not an object")?;
            servers.insert(args.name.clone(), entry);
        }
    }
    Ok(())
}

fn render_json_mcp_fragment(args: &InstallMcpArgs) -> Result<String> {
    let entry = build_json_mcp_entry(args)?;
    let fragment =
        match json_mcp_location(args.client).context("internal: unsupported JSON MCP client")? {
            JsonMcpLocation::RootMcpServers => json!({
                "mcpServers": { args.name.as_str(): entry }
            }),
            JsonMcpLocation::RootMcp => json!({
                "mcp": { args.name.as_str(): entry }
            }),
            JsonMcpLocation::NestedMcpServers => json!({
                "mcp": { "servers": { args.name.as_str(): entry } }
            }),
        };
    Ok(serde_json::to_string_pretty(&fragment)?)
}

/// JSON entry shape used by Claude Code — `mcpServers.<name>` with
/// `type: "http"` + `url` plus optional `headers` (or the session-aware
/// stdio bridge).
fn build_mcp_entry(args: &InstallMcpArgs) -> Result<serde_json::Value> {
    let bearer = bearer_header_value(args.auth_token.as_deref());
    // `run()` resolves the URL before dispatch; the fallback only fires for
    // direct callers (tests, uninstall re-render) that skip that step.
    let server_url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL);
    let mut entry = serde_json::Map::new();
    match args.client {
        McpClient::ClaudeCode => {
            if args.session_aware {
                entry.insert("type".into(), json!("stdio"));
                entry.insert("command".into(), json!("ai-memory"));
                entry.insert(
                    "args".into(),
                    json!(["mcp-bridge", "--server-url", server_url]),
                );
                if let Some(token) = &args.auth_token {
                    entry.insert("env".into(), json!({"AI_MEMORY_AUTH_TOKEN": token}));
                }
            } else {
                entry.insert("type".into(), json!("http"));
                entry.insert("url".into(), json!(server_url));
                if let Some(b) = &bearer {
                    entry.insert("headers".into(), json!({"Authorization": b}));
                }
            }
        }
        _ => bail!("internal: build_mcp_entry called for unsupported client"),
    }
    Ok(serde_json::Value::Object(entry))
}

fn build_mcp_entry_opencode(args: &InstallMcpArgs) -> Result<serde_json::Value> {
    let bearer = bearer_header_value(args.auth_token.as_deref());
    let server_url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL);
    let mut entry = serde_json::Map::new();
    entry.insert("type".into(), json!("remote"));
    entry.insert("url".into(), json!(server_url));
    entry.insert("enabled".into(), json!(true));
    if let Some(b) = bearer {
        entry.insert("headers".into(), json!({"Authorization": b}));
    }
    Ok(serde_json::Value::Object(entry))
}

/// OpenCode 2.0 beta MCP entry: `type: "remote"` + `url` + optional
/// `headers` under `mcp.servers`. V2 has no `enabled` field (servers
/// connect unless `disabled: true`), and header-credentialed servers
/// must set `oauth: false` so the beta does not attempt OAuth discovery
/// against ai-memory's local endpoint
/// (https://opencode.ai/v2/docs/mcp-servers).
fn build_mcp_entry_opencode2(args: &InstallMcpArgs) -> Result<serde_json::Value> {
    let bearer = bearer_header_value(args.auth_token.as_deref());
    let server_url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL);
    let mut entry = serde_json::Map::new();
    entry.insert("type".into(), json!("remote"));
    entry.insert("url".into(), json!(server_url));
    // `false`, not absence: without it the beta probes the endpoint for
    // OAuth metadata on every connect.
    entry.insert("oauth".into(), json!(false));
    if let Some(b) = bearer {
        entry.insert("headers".into(), json!({"Authorization": b}));
    }
    Ok(serde_json::Value::Object(entry))
}

/// ZCode MCP entry: `type: "http"` + `url` + optional `headers` under
/// `~/.zcode/cli/config.json`'s `mcp.servers` map. ZCode's entry schema
/// is strict — an entry carrying any key outside `type`/`url`/`headers`/
/// `enabled`/`timeoutMs` is dropped silently — so this deliberately
/// emits nothing else (issue #511).
fn build_mcp_entry_zcode(args: &InstallMcpArgs) -> Result<serde_json::Value> {
    let bearer = bearer_header_value(args.auth_token.as_deref());
    let server_url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL);
    let mut entry = serde_json::Map::new();
    entry.insert("type".into(), json!("http"));
    entry.insert("url".into(), json!(server_url));
    if let Some(b) = bearer {
        entry.insert("headers".into(), json!({"Authorization": b}));
    }
    Ok(serde_json::Value::Object(entry))
}

fn render_claude_code(args: &InstallMcpArgs, config_path: &Path) -> Result<String> {
    let bearer = bearer_header_value(args.auth_token.as_deref());
    let cli_line = if args.session_aware {
        let env = args
            .auth_token
            .as_deref()
            .map(|token| format!(" --env \"AI_MEMORY_AUTH_TOKEN={token}\""))
            .unwrap_or_default();
        format!(
            "claude mcp add --transport stdio{env} {name} -- \\\n    ai-memory mcp-bridge --server-url {url}",
            name = args.name,
            url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL),
        )
    } else if let Some(b) = &bearer {
        format!(
            "claude mcp add --transport http {name} {url} \\\n    --header \"Authorization: {b}\"",
            name = args.name,
            url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL),
            b = b,
        )
    } else {
        format!(
            "claude mcp add --transport http {name} {url}",
            name = args.name,
            url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL),
        )
    };
    let snippet = render_json_mcp_fragment(args)?;
    Ok(format!(
        "# Claude Code — register the MCP server\n\
         #\n\
         # Recommended (one-shot CLI):\n\
         {cli_line}\n\
         #\n\
         # Equivalent JSON if you'd rather edit {config_path} directly:\n\
         {snippet}\n",
        config_path = config_path.display(),
    ))
}

fn render_opencode(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# OpenCode — add to ~/.config/opencode/opencode.json under \"mcp\":\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}

fn render_opencode2(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# OpenCode 2.0 beta (`opencode2`) — merge into\n\
         # ~/.config/opencode/opencode.json(c) under \"mcp\" → \"servers\":\n\
         #\n\
         # V2 nests servers under `mcp.servers` (v1 used top-level `mcp`)\n\
         # and has no `enabled` field. Both keys coexist in the one file,\n\
         # so v1 and the beta stay wired side by side. If your config is\n\
         # `opencode.jsonc` with comments, re-run with\n\
         # `--config-file ~/.config/opencode/opencode.jsonc`.\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}

fn render_zcode(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# ZCode (z.ai) — merge into ~/.zcode/cli/config.json\n\
         # (or re-run this command with --apply), then restart ZCode.\n\
         #\n\
         # The entry schema is strict: keys outside type/url/headers/\n\
         # enabled/timeoutMs make ZCode drop the server silently.\n\
         # ai-memory's default stateless /mcp endpoint needs no flavor\n\
         # marker; auth goes in the headers map.\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::fs;
    use tempfile;

    fn args_for(client: McpClient) -> InstallMcpArgs {
        InstallMcpArgs {
            client,
            server_url: None,
            name: "ai-memory".into(),
            auth_token: None,
            apply: false,
            config_file: None,
            session_aware: false,
        }
    }

    fn args_with_token(client: McpClient) -> InstallMcpArgs {
        InstallMcpArgs {
            client,
            server_url: None,
            name: "ai-memory".into(),
            auth_token: Some("test-token-deadbeef".into()),
            apply: false,
            config_file: None,
            session_aware: false,
        }
    }

    #[test]
    fn opencode2_entry_uses_v2_servers_shape() {
        // V2 nests under `mcp.servers`, drops v1's `enabled`, and disables
        // OAuth discovery: ai-memory authenticates with a static header.
        let entry = build_json_mcp_entry(&args_with_token(McpClient::OpenCode2)).unwrap();
        assert_eq!(entry["type"], json!("remote"));
        assert_eq!(entry["url"], json!("http://127.0.0.1:49374/mcp"));
        assert_eq!(entry["oauth"], json!(false));
        assert!(entry.get("enabled").is_none());
        assert_eq!(
            entry["headers"]["Authorization"],
            json!("Bearer test-token-deadbeef")
        );

        let mut root = serde_json::Map::new();
        upsert_json_mcp_entry(&mut root, &args_with_token(McpClient::OpenCode2)).unwrap();
        assert!(root["mcp"]["servers"]["ai-memory"].is_object());
        assert!(root.get("mcpServers").is_none());

        // v1 still lands under top-level `mcp`, so both stay wired.
        let mut v1 = serde_json::Map::new();
        upsert_json_mcp_entry(&mut v1, &args_with_token(McpClient::OpenCode)).unwrap();
        assert!(v1["mcp"]["ai-memory"].is_object());
        assert!(v1["mcp"].get("servers").is_none());
    }

    #[test]
    fn opencode2_render_points_at_v2_key() {
        let out = render_opencode2(&args_for(McpClient::OpenCode2)).unwrap();
        assert!(out.contains("opencode2"));
        assert!(out.contains("mcp"));
        assert!(out.contains("servers"));
        assert!(out.contains("http://127.0.0.1:49374/mcp"));
    }

    #[test]
    fn claude_code_config_path_honours_claude_config_dir() {
        let custom = if cfg!(windows) {
            r"C:\custom\claude"
        } else {
            "/custom/claude"
        };
        let path = claude_code_config_path_in(Some(std::ffi::OsString::from(custom))).unwrap();
        assert_eq!(path, std::path::Path::new(custom).join(".claude.json"));

        // Empty override and unset var both fall back to ~/.claude.json.
        for env in [None, Some(std::ffi::OsString::new())] {
            let path = claude_code_config_path_in(env).unwrap();
            assert!(
                path.ends_with(".claude.json") && !path.starts_with(custom),
                "default must be ~/.claude.json, got {}",
                path.display()
            );
        }
    }

    #[test]
    fn claude_code_render_shows_resolved_config_path() {
        let args = args_for(McpClient::ClaudeCode);
        let config_path = std::path::Path::new("/stores/claude/.claude.json");
        let out = render_claude_code(&args, config_path).unwrap();
        assert!(
            out.contains("/stores/claude/.claude.json"),
            "render must mention the resolved config path:\n{out}"
        );
        assert!(
            !out.contains("~/.claude.json"),
            "render must not hardcode ~/.claude.json when the resolved path differs:\n{out}"
        );
    }

    #[test]
    fn claude_code_session_aware_entry_uses_owned_stdio_bridge() {
        let mut args = args_with_token(McpClient::ClaudeCode);
        args.server_url = Some("https://memory.example/mcp".into());
        args.session_aware = true;

        let entry = build_mcp_entry(&args).unwrap();

        assert_eq!(entry["type"], "stdio");
        assert_eq!(entry["command"], "ai-memory");
        assert_eq!(
            entry["args"],
            json!(["mcp-bridge", "--server-url", "https://memory.example/mcp"])
        );
        assert_eq!(entry["env"]["AI_MEMORY_AUTH_TOKEN"], "test-token-deadbeef");
        assert!(entry.get("url").is_none());
        assert!(entry.get("headers").is_none());

        let rendered = render_claude_code(&args, Path::new("/home/alice/.claude.json")).unwrap();
        assert!(rendered.contains("claude mcp add --transport stdio"));
        assert!(rendered.contains("ai-memory mcp-bridge --server-url"));
    }

    #[test]
    fn claude_code_session_aware_apply_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_file = tmp.path().join(".claude.json");
        let mut args = args_with_token(McpClient::ClaudeCode);
        args.server_url = Some("http://192.168.0.90:49374/mcp".into());
        args.config_file = Some(config_file.clone());
        args.session_aware = true;
        args.apply = true;

        apply_to_config_file(&args).unwrap();
        let first = fs::read_to_string(&config_file).unwrap();
        apply_to_config_file(&args).unwrap();
        let second = fs::read_to_string(&config_file).unwrap();

        assert_eq!(first, second);
        let value: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(
            value["mcpServers"]["ai-memory"]["args"],
            json!([
                "mcp-bridge",
                "--server-url",
                "http://192.168.0.90:49374/mcp"
            ])
        );
    }

    #[test]
    fn session_aware_rejects_non_claude_clients() {
        let mut args = args_for(McpClient::OpenCode);
        args.session_aware = true;

        let error = build_json_mcp_entry(&args).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("supported only for --client claude-code"),
            "{error:#}"
        );
    }

    fn render_with_token(client: McpClient) -> String {
        let args = args_with_token(client);
        match args.client {
            McpClient::ClaudeCode => {
                render_claude_code(&args, Path::new("/home/alice/.claude.json")).unwrap()
            }
            McpClient::OpenCode => render_opencode(&args).unwrap(),
            McpClient::OpenCode2 => render_opencode2(&args).unwrap(),
            McpClient::Zcode => render_zcode(&args).unwrap(),
        }
    }

    /// With `--auth-token` set, every renderer must embed the Bearer
    /// header in its output.
    #[test]
    fn auth_token_threaded_into_every_client() {
        for client in [
            McpClient::ClaudeCode,
            McpClient::OpenCode,
            McpClient::OpenCode2,
            McpClient::Zcode,
        ] {
            let out = render_with_token(client);
            // Every client embeds the token as `Authorization:
            // Bearer <token>` in some flavour of headers map — the
            // exact key path differs (Codex uses `http_headers`,
            // OpenCode uses `headers`, Cursor / Gemini / Claude
            // Desktop / Claude Code use `headers` inside their
            // server entry, etc.), but the literal `Bearer
            // <token>` substring shows up in all of them. Keep
            // the assertion uniform.
            assert!(
                out.contains("Bearer test-token-deadbeef"),
                "client {client:?} did not embed the bearer token:\n{out}"
            );
        }
    }

    /// Sanity: every supported client renders without error and the
    /// output mentions the configured server URL.
    #[test]
    fn every_client_renders() {
        for client in [
            McpClient::ClaudeCode,
            McpClient::OpenCode,
            McpClient::OpenCode2,
            McpClient::Zcode,
        ] {
            let out = render_for_test(client);
            assert!(
                out.contains("http://127.0.0.1:49374/mcp"),
                "client {client:?} did not include the server URL in output:\n{out}"
            );
        }
    }

    fn render_for_test(client: McpClient) -> String {
        let args = args_for(client);
        match args.client {
            McpClient::ClaudeCode => {
                render_claude_code(&args, Path::new("/home/alice/.claude.json")).unwrap()
            }
            McpClient::OpenCode => render_opencode(&args).unwrap(),
            McpClient::OpenCode2 => render_opencode2(&args).unwrap(),
            McpClient::Zcode => render_zcode(&args).unwrap(),
        }
    }

    #[test]
    fn mcp_server_url_defaults_to_configured_server_url() {
        let config = Config {
            server_url: "http://192.168.0.90:49374/".into(),
            ..Config::default()
        };
        let args = args_for(McpClient::OpenCode);

        assert_eq!(
            effective_mcp_server_url(&config, &args),
            "http://192.168.0.90:49374/mcp"
        );
    }

    #[test]
    fn mcp_server_url_does_not_duplicate_mcp_suffix() {
        let config = Config {
            server_url: "http://192.168.0.90:49374/mcp".into(),
            ..Config::default()
        };
        let args = args_for(McpClient::OpenCode);

        assert_eq!(
            effective_mcp_server_url(&config, &args),
            "http://192.168.0.90:49374/mcp"
        );
    }

    /// Regression for #185: an explicit `--server-url` passed as a BASE url
    /// (the same value `install-hooks --server-url` takes) must gain the
    /// `/mcp` suffix, or every client renderer emits a config pointing at
    /// the server root, which 404s. Trailing slashes are trimmed first.
    #[test]
    fn mcp_server_url_explicit_base_url_gains_mcp_suffix() {
        let config = Config::default();
        let mut args = args_for(McpClient::ClaudeCode);
        args.server_url = Some("https://memory.example.com".into());
        assert_eq!(
            effective_mcp_server_url(&config, &args),
            "https://memory.example.com/mcp"
        );

        args.server_url = Some("https://memory.example.com/".into());
        assert_eq!(
            effective_mcp_server_url(&config, &args),
            "https://memory.example.com/mcp"
        );

        // A reverse-proxy base path keeps its prefix.
        args.server_url = Some("https://host/prefix".into());
        assert_eq!(
            effective_mcp_server_url(&config, &args),
            "https://host/prefix/mcp"
        );
    }

    #[test]
    fn mcp_server_url_explicit_flag_wins_over_config() {
        let config = Config {
            server_url: "http://homelab:49374".into(),
            ..Config::default()
        };
        let mut args = args_for(McpClient::OpenCode);
        args.server_url = Some("http://explicit:49374/mcp".into());

        assert_eq!(
            effective_mcp_server_url(&config, &args),
            "http://explicit:49374/mcp"
        );
    }

    /// Regression (found 2026-07-12 during real-acceptance A/B
    /// testing): an explicit `--server-url` that happens to equal the
    /// compiled-in `DEFAULT_MCP_URL` must still win over a configured
    /// (env/config.toml) server_url pointing somewhere else. Mirrors
    /// `hook_server_url_explicit_flag_matching_compiled_default_still_wins`
    /// in install_hooks.rs -- same bug class, same fix, both commands.
    #[test]
    fn mcp_server_url_explicit_flag_matching_compiled_default_still_wins() {
        let config = Config {
            server_url: "http://127.0.0.1:49375".into(),
            ..Config::default()
        };
        let mut args = args_for(McpClient::OpenCode);
        args.server_url = Some(DEFAULT_MCP_URL.to_string());

        assert_eq!(
            effective_mcp_server_url(&config, &args),
            DEFAULT_MCP_URL,
            "an explicit --server-url matching the compiled default must not be \
             silently overridden by a differently-configured server_url"
        );
    }

    /// Specific shape checks — each client has a distinguishing key
    /// in its JSON snippet. This catches accidental cross-pollination
    /// between renderers (e.g. Gemini's `httpUrl` showing up under
    /// Cursor's `mcpServers`).
    #[test]
    fn client_specific_shape_keys() {
        // Claude Code writes mcpServers.<name> with type http + url
        // (or the session-aware stdio bridge).
        let claude = render_for_test(McpClient::ClaudeCode);
        assert!(claude.contains("\"mcpServers\""));
        assert!(claude.contains("\"type\": \"http\""));
        assert!(claude.contains(".claude.json"));
        // OpenCode v1 reads the top-level mcp map with type remote.
        let opencode = render_for_test(McpClient::OpenCode);
        assert!(opencode.contains("\"mcp\""));
        assert!(opencode.contains("\"type\": \"remote\""));
        assert!(opencode.contains("\"enabled\": true"));
        // OpenCode 2 nests under mcp.servers, has no enabled, and must
        // disable OAuth discovery for header-credentialed servers.
        let opencode2 = render_for_test(McpClient::OpenCode2);
        assert!(opencode2.contains("\"servers\""));
        assert!(!opencode2.contains("\"enabled\""));
        assert!(opencode2.contains("\"oauth\": false"));
        // ZCode schema is strict: only type/url/headers (+enabled/timeoutMs).
        let zcode = render_for_test(McpClient::Zcode);
        assert!(zcode.contains("\"mcp\""));
        assert!(zcode.contains("\"servers\""));
        assert!(zcode.contains("\"type\": \"http\""));
        assert!(!zcode.contains("\"transport\""));
        let zcode_with_token = render_with_token(McpClient::Zcode);
        assert!(zcode_with_token.contains("\"Authorization\": \"Bearer test-token-deadbeef\""));
    }

    #[test]
    fn zcode_mcp_config_path_pins_user_scope_location() {
        assert_eq!(
            mcp_config_path(McpClient::Zcode).unwrap(),
            home_dir()
                .unwrap()
                .join(".zcode")
                .join("cli")
                .join("config.json")
        );
    }

    /// ZCode's entry schema is strict — any key outside
    /// type/url/headers/enabled/timeoutMs makes it drop the server
    /// silently — so the fragment must carry exactly the documented
    /// keys, nested under `mcp.servers` (#511).
    #[test]
    fn zcode_renderer_nests_strict_http_entry_under_mcp_servers() {
        let fragment = render_json_mcp_fragment(&args_with_token(McpClient::Zcode)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&fragment).unwrap();

        assert_eq!(
            value,
            json!({
                "mcp": {
                    "servers": {
                        "ai-memory": {
                            "type": "http",
                            "url": "http://127.0.0.1:49374/mcp",
                            "headers": {
                                "Authorization": "Bearer test-token-deadbeef"
                            }
                        }
                    }
                }
            })
        );
        let rendered = render_zcode(&args_for(McpClient::Zcode)).unwrap();
        assert!(rendered.contains("~/.zcode/cli/config.json"));
        assert!(rendered.contains("strict"));
    }

    /// `--apply` merges under `mcp.servers` keeping the rest of the
    /// config and sibling servers intact, and re-runs are a no-op.
    #[test]
    fn zcode_apply_preserves_siblings_and_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.json");
        fs::write(
            &config_path,
            r#"{
  "theme": "dark",
  "mcp": {
    "servers": {
      "other": {"type": "http", "url": "https://other.example/mcp"}
    }
  }
}"#,
        )
        .unwrap();
        let mut args = args_with_token(McpClient::Zcode);
        args.config_file = Some(config_path.clone());

        apply_to_config_file(&args).unwrap();
        let first = fs::read_to_string(&config_path).unwrap();
        apply_to_config_file(&args).unwrap();
        let second = fs::read_to_string(&config_path).unwrap();

        assert_eq!(first, second);
        let value: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(
            value["mcp"]["servers"]["other"]["url"], "https://other.example/mcp",
            "install must preserve sibling servers"
        );
        assert_eq!(value["mcp"]["servers"]["ai-memory"]["type"], "http");
        assert_eq!(
            value["mcp"]["servers"]["ai-memory"]["headers"]["Authorization"],
            "Bearer test-token-deadbeef"
        );
    }

    /// Pin the append rules: `?` on a bare endpoint, `&` with an existing
    /// query, never duplicate an existing marker.
