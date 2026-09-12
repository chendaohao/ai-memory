//! Incremental, read-only extraction from native harness session stores.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ai_memory_core::{
    AgentKind, MANAGED_WORKSTREAM_PACKET_MARKER, NewWorkstreamEvent, WorkstreamEventKind,
};
use anyhow::{Context as _, Result, anyhow};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::ManagedHarness;

const MAX_SCAN_FILES: usize = 50_000;
const MAX_EVENT_BYTES: usize = 128 * 1024;
const MAX_NATIVE_SESSION_ID_BYTES: usize = 512;
const LEGACY_MANAGED_WORKSTREAM_PACKET_PREFIX: &str = "> **ai-memory managed workstream:";

/// Checkout-local native session that can seed an otherwise-empty workstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSessionCandidate {
    /// Harness-native session identifier.
    pub native_session_id: String,
    /// Last observed native-store update time.
    pub updated_at: SystemTime,
}

/// Incremental transcript export produced after a managed child exits.
#[derive(Debug, Clone, Default)]
pub struct ExportedTranscript {
    /// Native session that was read.
    pub native_session_id: String,
    /// Opaque adapter cursor persisted only for the next local read.
    pub source_cursor: Option<String>,
    /// Portable visible events after the incoming cursor.
    pub events: Vec<NewWorkstreamEvent>,
    /// Explicit records of private, malformed, or unsupported source data.
    pub losses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileCursor {
    path: String,
    offset: u64,
    /// Hash of every committed byte through `offset`, written by adapters
    /// whose journals can be rewritten in place. None of the kept harnesses
    /// do, so the field stays reserved for forward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prefix_sha256: Option<String>,
}

pub async fn export_transcript(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
    native_session_id: &str,
    source_cursor: Option<&str>,
) -> Result<ExportedTranscript> {
    if harness == ManagedHarness::OpenCode {
        return export_opencode(home, session_dir, native_session_id, source_cursor);
    }
    if harness == ManagedHarness::OpenCode2 {
        return export_opencode2(home, session_dir, native_session_id, source_cursor);
    }
    let path = locate_session_file(harness, home, cwd, session_dir, native_session_id)?
        .ok_or_else(|| anyhow!("native transcript for {native_session_id} was not found"))?;
    export_jsonl(harness, &path, native_session_id, source_cursor)
}

/// Discover a session created after `started_at` when the harness could not be
/// assigned an id before launch and SessionStart did not link one.
pub async fn discover_native_session(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
    started_at: SystemTime,
) -> Result<Option<String>> {
    if harness == ManagedHarness::OpenCode {
        return discover_opencode(home, session_dir, cwd, started_at);
    }
    if harness == ManagedHarness::OpenCode2 {
        return discover_opencode2(home, session_dir, cwd, started_at);
    }
    let mut candidates = collect_session_files(harness, home, session_dir)?;
    candidates.sort_by(|left, right| {
        modified(right)
            .cmp(&modified(left))
            .then_with(|| left.cmp(right))
    });
    for path in candidates.into_iter().take(512) {
        if modified(&path).is_some_and(|time| time + Duration::from_secs(2) < started_at) {
            break;
        }
        if let Some((id, record_cwd)) = session_header_for_cwd(harness, &path, cwd)?
            && same_path(&record_cwd, cwd)
        {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// List newest native sessions whose recorded working directory matches the
/// current checkout. Native stores are opened read-only and unrelated paths are
/// excluded before candidates reach the launcher prompt.
pub async fn list_native_sessions(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
    limit: usize,
) -> Result<Vec<NativeSessionCandidate>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    if harness == ManagedHarness::OpenCode {
        return list_opencode_sessions(home, session_dir, cwd, limit);
    }
    if harness == ManagedHarness::OpenCode2 {
        return list_opencode2_sessions(home, session_dir, cwd, limit);
    }
    let mut files = collect_session_files(harness, home, session_dir)?;
    files.sort_by(|left, right| {
        modified(right)
            .cmp(&modified(left))
            .then_with(|| left.cmp(right))
    });
    let mut seen = HashSet::new();
    let mut sessions = Vec::new();
    for path in files.into_iter().take(2_000) {
        let Some(updated_at) = modified(&path) else {
            continue;
        };
        let Ok(Some((native_session_id, recorded_cwd))) =
            session_header_for_cwd(harness, &path, cwd)
        else {
            continue;
        };
        if !same_path(&recorded_cwd, cwd)
            || !valid_native_session_id(&native_session_id)
            || !seen.insert(native_session_id.clone())
        {
            continue;
        }
        sessions.push(NativeSessionCandidate {
            native_session_id,
            updated_at,
        });
        if sessions.len() >= limit {
            break;
        }
    }
    Ok(sessions)
}

/// Check whether one exact native session still exists in the harness's
/// read-only transcript store. `Ok(false)` means the resume target is
/// definitely absent; store access or schema failures remain errors so callers
/// do not mistake an unreadable store for a deleted session.
pub fn native_session_exists(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
    native_session_id: &str,
) -> Result<bool> {
    if harness == ManagedHarness::OpenCode {
        return Ok(opencode_updated(home, session_dir, native_session_id)?.is_some());
    }
    if harness == ManagedHarness::OpenCode2 {
        return Ok(opencode2_updated(home, session_dir, native_session_id)?.is_some());
    }
    Ok(locate_session_file(harness, home, cwd, session_dir, native_session_id)?.is_some())
}

/// Wait briefly for buffered transcript writers to settle before importing.
pub async fn wait_for_transcript_flush(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
    native_session_id: &str,
) -> Result<()> {
    let mut previous = None;
    for _ in 0..10 {
        let current = if harness == ManagedHarness::OpenCode {
            opencode_updated(home, session_dir, native_session_id)?.map(|value| value.to_string())
        } else if harness == ManagedHarness::OpenCode2 {
            opencode2_updated(home, session_dir, native_session_id)?.map(|value| value.to_string())
        } else {
            locate_session_file(harness, home, cwd, session_dir, native_session_id)?.and_then(
                |path| {
                    fs::metadata(&path)
                        .ok()
                        .map(|metadata| format!("{}:{}", path.display(), metadata.len()))
                },
            )
        };
        if current.is_some() && current == previous {
            return Ok(());
        }
        previous = current;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

fn export_jsonl(
    harness: ManagedHarness,
    path: &Path,
    native_session_id: &str,
    source_cursor: Option<&str>,
) -> Result<ExportedTranscript> {
    let cursor = source_cursor
        .and_then(|raw| serde_json::from_str::<FileCursor>(raw).ok())
        .filter(|cursor| Path::new(&cursor.path) == path);
    let mut file = File::open(path)
        .with_context(|| format!("opening native transcript {}", path.display()))?;
    let len = file.metadata()?.len();
    let start = cursor.map_or(0, |cursor| cursor.offset.min(len));
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    let mut offset = start;
    let mut committed_offset = start;
    let mut line = Vec::new();
    let mut events = Vec::new();
    let mut losses = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        offset += read as u64;
        if !line.ends_with(b"\n") {
            break;
        }
        committed_offset = offset;
        let value: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(_) => {
                losses.push(format!(
                    "malformed JSONL record at byte {}",
                    offset - read as u64
                ));
                continue;
            }
        };
        let record_id =
            source_id(&value).unwrap_or_else(|| format!("byte-{}", offset - read as u64));
        match harness {
            ManagedHarness::Claude => parse_claude(
                &value,
                native_session_id,
                &record_id,
                &mut events,
                &mut losses,
            ),
            ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => {
                return Err(anyhow!(
                    "{} transcripts must use their SQLite adapter",
                    harness.as_str()
                ));
            }
        }
    }
    Ok(ExportedTranscript {
        native_session_id: native_session_id.to_string(),
        source_cursor: Some(serde_json::to_string(&FileCursor {
            path: path.to_string_lossy().into_owned(),
            offset: committed_offset,
            prefix_sha256: None,
        })?),
        events,
        losses: deduplicate_losses(losses),
    })
}


fn parse_claude(
    value: &Value,
    session: &str,
    record_id: &str,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
        losses.push("Claude synthetic/meta records were intentionally excluded".into());
        return;
    }
    let record_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let compact_boundary = record_type == "system"
        && value.get("subtype").and_then(Value::as_str) == Some("compact_boundary");
    if compact_boundary || matches!(record_type, "summary" | "compact" | "compaction") {
        if let Some(text) = first_string(value, &["summary", "content", "text"]) {
            push_event(
                events,
                AgentKind::ClaudeCode,
                session,
                record_id,
                0,
                WorkstreamEventKind::Compaction,
                Some("assistant"),
                text,
                timestamp(value),
                json!({}),
            );
        }
        return;
    }
    if !matches!(record_type, "user" | "assistant") {
        return;
    }
    let message = value.get("message").unwrap_or(value);
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or(record_type);
    if !matches!(role, "user" | "assistant") {
        losses.push("Claude non-conversation message records were intentionally excluded".into());
        return;
    }
    parse_content_blocks(
        AgentKind::ClaudeCode,
        session,
        record_id,
        role,
        message.get("content"),
        timestamp(value),
        events,
        losses,
    );
}

fn parse_content_blocks(
    agent: AgentKind,
    session: &str,
    record_id: &str,
    role: &str,
    content: Option<&Value>,
    occurred_at: Option<String>,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    let Some(content) = content else { return };
    let blocks: Vec<&Value> = content
        .as_array()
        .map_or_else(|| vec![content], |items| items.iter().collect());
    for (index, block) in blocks.into_iter().enumerate() {
        if let Some(text) = block.as_str() {
            if codex_synthetic_context(agent, role, text) {
                continue;
            }
            push_event(
                events,
                agent,
                session,
                record_id,
                index,
                WorkstreamEventKind::Message,
                Some(role),
                text,
                occurred_at.clone(),
                json!({}),
            );
            continue;
        }
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match block_type {
            "text" | "input_text" | "output_text" => {
                if let Some(text) = first_string(block, &["text", "content"]) {
                    if codex_synthetic_context(agent, role, text) {
                        continue;
                    }
                    push_event(
                        events,
                        agent,
                        session,
                        record_id,
                        index,
                        WorkstreamEventKind::Message,
                        Some(role),
                        text,
                        occurred_at.clone(),
                        json!({}),
                    );
                }
            }
            "tool_use" | "toolCall" | "tool_call" => {
                let name = first_string(block, &["name", "toolName", "tool"]).unwrap_or("tool");
                let input = block
                    .get("input")
                    .or_else(|| block.get("arguments"))
                    .map(compact_json)
                    .unwrap_or_default();
                push_event(
                    events,
                    agent,
                    session,
                    record_id,
                    index,
                    WorkstreamEventKind::ToolCall,
                    Some("assistant"),
                    &format!("{name}: {input}"),
                    occurred_at.clone(),
                    json!({"tool": name}),
                );
            }
            "tool_result" | "toolResult" => {
                let body = block.get("content").map(value_text).unwrap_or_default();
                if claude_managed_packet_echo(agent, &body) {
                    losses.push(
                        "Claude managed workstream delivery packets were intentionally excluded"
                            .into(),
                    );
                    continue;
                }
                push_event(
                    events,
                    agent,
                    session,
                    record_id,
                    index,
                    WorkstreamEventKind::ToolResult,
                    Some("tool"),
                    &body,
                    occurred_at.clone(),
                    json!({"is_error": block.get("is_error").and_then(Value::as_bool).unwrap_or(false)}),
                );
            }
            "thinking" | "reasoning" | "redacted_thinking" => {
                losses.push(format!(
                    "{} hidden reasoning was intentionally excluded",
                    agent.as_str()
                ));
            }
            _ => {}
        }
    }
}

fn claude_managed_packet_echo(agent: AgentKind, body: &str) -> bool {
    if agent != AgentKind::ClaudeCode {
        return false;
    }
    let body = body.trim_start();
    body.starts_with(MANAGED_WORKSTREAM_PACKET_MARKER)
        || body.starts_with(LEGACY_MANAGED_WORKSTREAM_PACKET_PREFIX)
}

fn codex_synthetic_context(agent: AgentKind, role: &str, text: &str) -> bool {
    if role != "user" {
        return false;
    }
    let trimmed = text.trim_start();
    match agent {
        AgentKind::Codex => {
            trimmed.starts_with("# AGENTS.md instructions for ")
                || trimmed.starts_with("<environment_context>")
                || trimmed.starts_with("<permissions instructions>")
                || trimmed.starts_with("<INSTRUCTIONS>")
        }
        AgentKind::ClaudeCode => trimmed.starts_with("<system-reminder>"),
        // Grok stores harness scaffolding inside `user` records rather than a
        // separate record type: an environment block, then Claude-style
        // reminders carrying project instructions, the skills catalogue, and
        // the connected MCP servers. A single session's reminders measured
        // 42KB against 270 bytes of real input, so importing them both leaks
        // harness internals into the portable ledger and evicts real
        // conversation from the startup packet budget. Genuine input arrives
        // wrapped in `<user_query>`.
        AgentKind::Grok => {
            trimmed.starts_with("<user_info>") || trimmed.starts_with("<system-reminder>")
        }
        _ => false,
    }
}

fn export_opencode(
    home: &Path,
    session_dir: Option<&Path>,
    session: &str,
    source_cursor: Option<&str>,
) -> Result<ExportedTranscript> {
    let db = opencode_db(home, session_dir);
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "opening OpenCode session database {} read-only",
            db.display()
        )
    })?;
    let cursor = source_cursor
        .and_then(|raw| serde_json::from_str::<SqlCursor>(raw).ok())
        .unwrap_or_default();
    let mut statement = connection.prepare(
        "SELECT p.id, p.time_updated, m.data, p.data
         FROM part p JOIN message m ON m.id = p.message_id
         WHERE p.session_id = ?1 AND (p.time_updated > ?2 OR (p.time_updated = ?2 AND p.id > ?3))
         ORDER BY p.time_updated, p.id",
    )?;
    let rows = statement.query_map(params![session, cursor.updated, cursor.id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut events = Vec::new();
    let mut losses = Vec::new();
    let mut next_cursor = cursor;
    for row in rows {
        let (id, updated, message_raw, part_raw) = row?;
        next_cursor = SqlCursor {
            updated,
            id: id.clone(),
        };
        let Ok(message) = serde_json::from_str::<Value>(&message_raw) else {
            losses.push(format!("malformed OpenCode message for part {id}"));
            continue;
        };
        let Ok(part) = serde_json::from_str::<Value>(&part_raw) else {
            losses.push(format!("malformed OpenCode part {id}"));
            continue;
        };
        parse_opencode(&message, &part, session, &id, &mut events, &mut losses);
    }
    Ok(ExportedTranscript {
        native_session_id: session.to_string(),
        source_cursor: Some(serde_json::to_string(&next_cursor)?),
        events,
        losses: deduplicate_losses(losses),
    })
}

fn parse_opencode(
    message: &Value,
    part: &Value,
    session: &str,
    id: &str,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("assistant");
    let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "text" => {
            if !matches!(role, "user" | "assistant") {
                losses.push(
                    "OpenCode non-conversation message records were intentionally excluded".into(),
                );
                return;
            }
            if let Some(text) = first_string(part, &["text", "content"]) {
                push_event(
                    events,
                    AgentKind::OpenCode,
                    session,
                    id,
                    0,
                    WorkstreamEventKind::Message,
                    Some(role),
                    text,
                    None,
                    json!({}),
                );
            }
        }
        "tool" => {
            let name = first_string(part, &["tool", "name"]).unwrap_or("tool");
            let state = part.get("state").unwrap_or(&Value::Null);
            let input = state.get("input").map(compact_json).unwrap_or_default();
            push_event(
                events,
                AgentKind::OpenCode,
                session,
                id,
                0,
                WorkstreamEventKind::ToolCall,
                Some("assistant"),
                &format!("{name}: {input}"),
                None,
                json!({"tool": name}),
            );
            if let Some(output) = state
                .get("output")
                .map(value_text)
                .filter(|value| !value.is_empty())
            {
                push_event(
                    events,
                    AgentKind::OpenCode,
                    session,
                    id,
                    1,
                    WorkstreamEventKind::ToolResult,
                    Some("tool"),
                    &output,
                    None,
                    json!({"status": state.get("status").and_then(Value::as_str)}),
                );
            }
        }
        "compaction" => {
            let body = first_string(part, &["summary", "text", "content"]).unwrap_or("");
            push_event(
                events,
                AgentKind::OpenCode,
                session,
                id,
                0,
                WorkstreamEventKind::Compaction,
                Some("assistant"),
                body,
                None,
                json!({}),
            );
        }
        "reasoning" => losses.push("OpenCode hidden reasoning was intentionally excluded".into()),
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn push_event(
    events: &mut Vec<NewWorkstreamEvent>,
    agent: AgentKind,
    session: &str,
    record_id: &str,
    block: usize,
    kind: WorkstreamEventKind,
    role: Option<&str>,
    content: &str,
    occurred_at: Option<String>,
    metadata: Value,
) {
    if content.trim().is_empty() {
        return;
    }
    let content = truncate_utf8(content, MAX_EVENT_BYTES);
    let seed = format!(
        "{}\0{session}\0{record_id}\0{block}\0{}\0{content}",
        agent.as_str(),
        kind.as_str()
    );
    events.push(NewWorkstreamEvent {
        event_id: format!("native:{:x}", Sha256::digest(seed.as_bytes())),
        agent,
        native_session_id: session.to_string(),
        source_record_id: Some(record_id.to_string()),
        kind,
        role: role.map(str::to_string),
        content: content.to_string(),
        occurred_at,
        metadata,
    });
}

fn locate_session_file(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
    id: &str,
) -> Result<Option<PathBuf>> {
    let root = session_root(harness, home, session_dir);
    if !valid_native_session_id(id) {
        return Ok(None);
    }
    if harness == ManagedHarness::Claude {
        let encoded = cwd.to_string_lossy().replace('/', "-");
        let exact = root.join(encoded).join(format!("{id}.jsonl"));
        if exact.is_file() {
            return Ok(Some(exact));
        }
    }
    let mut files = collect_session_files(harness, home, session_dir)?;
    files.sort_by_key(|path| temporary_transcript(path));
    for path in files.into_iter().take(2_000) {
        if session_path_matches(harness, &path, id, cwd)? {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn session_path_matches(
    harness: ManagedHarness,
    path: &Path,
    id: &str,
    cwd: &Path,
) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    Ok(session_header_for_cwd(harness, path, cwd)?
        .is_some_and(|(found, recorded_cwd)| found == id && same_path(&recorded_cwd, cwd)))
}

fn transcript_file(_harness: ManagedHarness, path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "jsonl")
}

fn temporary_transcript(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "tmp")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains(".jsonl."))
}

fn session_header(harness: ManagedHarness, path: &Path) -> Result<Option<(String, PathBuf)>> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = String::new();
    for _ in 0..64 {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (id, cwd) = match harness {
            ManagedHarness::Claude => (
                value.get("sessionId").and_then(Value::as_str),
                value.get("cwd").and_then(Value::as_str),
            ),
            ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => (None, None),
        };
        if let (Some(id), Some(cwd)) = (id, cwd) {
            return Ok(Some((id.to_string(), PathBuf::from(cwd))));
        }
    }
    Ok(None)
}

fn session_header_for_cwd(
    harness: ManagedHarness,
    path: &Path,
    _cwd: &Path,
) -> Result<Option<(String, PathBuf)>> {
    session_header(harness, path)
}

fn discover_opencode(
    home: &Path,
    session_dir: Option<&Path>,
    cwd: &Path,
    started_at: SystemTime,
) -> Result<Option<String>> {
    let db = opencode_db(home, session_dir);
    if !db.is_file() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let since = started_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let mut statement = connection.prepare(
        "SELECT id FROM session WHERE directory = ?1 AND time_updated >= ?2 ORDER BY time_updated DESC LIMIT 1",
    )?;
    match statement.query_row(params![cwd.to_string_lossy(), since], |row| row.get(0)) {
        Ok(id) => Ok(Some(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn list_opencode_sessions(
    home: &Path,
    session_dir: Option<&Path>,
    cwd: &Path,
    limit: usize,
) -> Result<Vec<NativeSessionCandidate>> {
    let db = opencode_db(home, session_dir);
    if !db.is_file() {
        return Ok(Vec::new());
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut statement = connection.prepare(
        "SELECT id, time_updated FROM session \
         WHERE directory = ?1 ORDER BY time_updated DESC LIMIT ?2",
    )?;
    let rows = statement.query_map(params![cwd.to_string_lossy(), limit as i64], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        let (native_session_id, updated_millis) = row?;
        let Ok(updated_millis) = u64::try_from(updated_millis) else {
            continue;
        };
        if !valid_native_session_id(&native_session_id) {
            continue;
        }
        let Some(updated_at) = UNIX_EPOCH.checked_add(Duration::from_millis(updated_millis)) else {
            continue;
        };
        sessions.push(NativeSessionCandidate {
            native_session_id,
            updated_at,
        });
    }
    Ok(sessions)
}

fn native_timestamp(value: i64) -> Option<SystemTime> {
    let value = u64::try_from(value).ok()?;
    if value < 100_000_000_000 {
        UNIX_EPOCH.checked_add(Duration::from_secs(value))
    } else {
        UNIX_EPOCH.checked_add(Duration::from_millis(value))
    }
}

fn opencode_updated(home: &Path, session_dir: Option<&Path>, session: &str) -> Result<Option<i64>> {
    let db = opencode_db(home, session_dir);
    if !db.is_file() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    match connection.query_row(
        "SELECT time_updated FROM session WHERE id = ?1",
        [session],
        |row| row.get(0),
    ) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn opencode_db(home: &Path, session_dir: Option<&Path>) -> PathBuf {
    session_dir.map_or_else(
        || home.join(".local/share/opencode/opencode.db"),
        |dir| dir.join("opencode.db"),
    )
}

/// OpenCode 2.0 beta transcript adapter.
///
/// The beta keeps v1's database file but replaced its tables: `session_v2`
/// carries sessions, `session_message` carries typed messages. Shapes below
/// were verified against beta-18999: user `{text, files}`, assistant
/// `content[]` with `text` / `tool` / `reasoning` parts. Reasoning content
/// is encrypted and excluded like v1's hidden reasoning; anything else
/// unrecognized becomes a bounded loss, never a guess — the beta schema is
/// still changing.
fn export_opencode2(
    home: &Path,
    session_dir: Option<&Path>,
    session: &str,
    source_cursor: Option<&str>,
) -> Result<ExportedTranscript> {
    let db = opencode_db(home, session_dir);
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "opening OpenCode session database {} read-only",
            db.display()
        )
    })?;
    let cursor = source_cursor
        .and_then(|raw| serde_json::from_str::<SqlCursor>(raw).ok())
        .unwrap_or_default();
    let mut statement = connection.prepare(
        "SELECT id, time_updated, type, data FROM session_message \
         WHERE session_id = ?1 AND (time_updated > ?2 OR (time_updated = ?2 AND id > ?3)) \
         ORDER BY time_updated, id",
    )?;
    let rows = statement.query_map(params![session, cursor.updated, cursor.id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut events = Vec::new();
    let mut losses = Vec::new();
    let mut next_cursor = cursor;
    for row in rows {
        let (id, updated, message_type, data_raw) = row?;
        next_cursor = SqlCursor {
            updated,
            id: id.clone(),
        };
        let Ok(data) = serde_json::from_str::<Value>(&data_raw) else {
            losses.push(format!("malformed OpenCode 2 message {id}"));
            continue;
        };
        parse_opencode2(&message_type, &data, session, &id, &mut events, &mut losses);
    }
    Ok(ExportedTranscript {
        native_session_id: session.to_string(),
        source_cursor: Some(serde_json::to_string(&next_cursor)?),
        events,
        losses: deduplicate_losses(losses),
    })
}

fn parse_opencode2(
    message_type: &str,
    data: &Value,
    session: &str,
    id: &str,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    match message_type {
        "user" => {
            if let Some(text) = data.get("text").and_then(Value::as_str) {
                push_event(
                    events,
                    AgentKind::OpenCode,
                    session,
                    id,
                    0,
                    WorkstreamEventKind::Message,
                    Some("user"),
                    text,
                    None,
                    json!({}),
                );
            }
        }
        "assistant" => {
            let Some(parts) = data.get("content").and_then(Value::as_array) else {
                losses.push(format!(
                    "OpenCode 2 assistant message {id} has no content parts"
                ));
                return;
            };
            for (index, part) in parts.iter().enumerate() {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
                // One message holds many parts; the block namespaces each
                // part's events deterministically (call before its result).
                let block = index * 2;
                match kind {
                    "text" => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            push_event(
                                events,
                                AgentKind::OpenCode,
                                session,
                                id,
                                block,
                                WorkstreamEventKind::Message,
                                Some("assistant"),
                                text,
                                None,
                                json!({}),
                            );
                        }
                    }
                    "tool" => {
                        let name = part.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let state = part.get("state").unwrap_or(&Value::Null);
                        let input = state.get("input").map(compact_json).unwrap_or_default();
                        push_event(
                            events,
                            AgentKind::OpenCode,
                            session,
                            id,
                            block,
                            WorkstreamEventKind::ToolCall,
                            Some("assistant"),
                            &format!("{name}: {input}"),
                            None,
                            json!({"tool": name}),
                        );
                        let status = state
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("completed");
                        if status == "completed" {
                            let output = state
                                .get("content")
                                .and_then(Value::as_array)
                                .map(|contents| {
                                    contents
                                        .iter()
                                        .filter_map(|item| item.get("text").and_then(Value::as_str))
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                })
                                .unwrap_or_default();
                            if !output.trim().is_empty() {
                                push_event(
                                    events,
                                    AgentKind::OpenCode,
                                    session,
                                    id,
                                    block + 1,
                                    WorkstreamEventKind::ToolResult,
                                    Some("tool"),
                                    &output,
                                    None,
                                    json!({"status": status}),
                                );
                            }
                        } else {
                            losses
                                .push(format!("OpenCode 2 tool {name} ended with status {status}"));
                        }
                    }
                    "reasoning" => {
                        losses.push("OpenCode 2 hidden reasoning was intentionally excluded".into())
                    }
                    other => losses.push(format!(
                        "OpenCode 2 {other} part was intentionally excluded"
                    )),
                }
            }
        }
        // System catalog notices and similar non-conversation records.
        _ => {}
    }
}

fn discover_opencode2(
    home: &Path,
    session_dir: Option<&Path>,
    cwd: &Path,
    started_at: SystemTime,
) -> Result<Option<String>> {
    let db = opencode_db(home, session_dir);
    if !db.is_file() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let since = started_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let mut statement = connection.prepare(
        "SELECT id FROM session_v2 WHERE directory = ?1 AND time_updated >= ?2 \
         ORDER BY time_updated DESC LIMIT 1",
    )?;
    match statement.query_row(params![cwd.to_string_lossy(), since], |row| row.get(0)) {
        Ok(id) => Ok(Some(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn list_opencode2_sessions(
    home: &Path,
    session_dir: Option<&Path>,
    cwd: &Path,
    limit: usize,
) -> Result<Vec<NativeSessionCandidate>> {
    let db = opencode_db(home, session_dir);
    if !db.is_file() {
        return Ok(Vec::new());
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut statement = connection.prepare(
        "SELECT id, time_updated FROM session_v2 \
         WHERE directory = ?1 ORDER BY time_updated DESC LIMIT ?2",
    )?;
    let rows = statement.query_map(params![cwd.to_string_lossy(), limit as i64], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        let (native_session_id, updated_millis) = row?;
        let Ok(updated_millis) = u64::try_from(updated_millis) else {
            continue;
        };
        if !valid_native_session_id(&native_session_id) {
            continue;
        }
        let Some(updated_at) = UNIX_EPOCH.checked_add(Duration::from_millis(updated_millis)) else {
            continue;
        };
        sessions.push(NativeSessionCandidate {
            native_session_id,
            updated_at,
        });
    }
    Ok(sessions)
}

fn opencode2_updated(
    home: &Path,
    session_dir: Option<&Path>,
    session: &str,
) -> Result<Option<i64>> {
    let db = opencode_db(home, session_dir);
    if !db.is_file() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    match connection.query_row(
        "SELECT time_updated FROM session_v2 WHERE id = ?1",
        [session],
        |row| row.get(0),
    ) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn session_root(harness: ManagedHarness, home: &Path, override_dir: Option<&Path>) -> PathBuf {
    if let Some(override_dir) = override_dir {
        return override_dir.to_path_buf();
    }
    match harness {
        ManagedHarness::Claude => home.join(".claude/projects"),
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => home.join(".local/share/opencode"),
    }
}

fn collect_session_files(
    harness: ManagedHarness,
    home: &Path,
    override_dir: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    let root = session_root(harness, home, override_dir);
    for path in collect_files(&root, |path| transcript_file(harness, path))? {
        if seen.insert(path.clone()) {
            files.push(path);
        }
    }
    Ok(files)
}

fn collect_files(root: &Path, predicate: impl Fn(&Path) -> bool + Copy) -> Result<Vec<PathBuf>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in
            fs::read_dir(&directory).with_context(|| format!("reading {}", directory.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(path)
            } else if file_type.is_file() && predicate(&path) {
                files.push(path);
                if files.len() >= MAX_SCAN_FILES {
                    return Ok(files);
                }
            }
        }
    }
    Ok(files)
}

fn source_id(value: &Value) -> Option<String> {
    for key in ["uuid", "id", "messageId", "call_id", "callId"] {
        if let Some(id) = value.get(key).and_then(Value::as_str) {
            return Some(id.to_string());
        }
    }
    value.get("payload").and_then(|payload| {
        ["id", "call_id", "callId"]
            .into_iter()
            .find_map(|key| payload.get(key).and_then(Value::as_str).map(str::to_string))
    })
}

fn timestamp(value: &Value) -> Option<String> {
    value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn first_string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(value_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => first_string(value, &["text", "content", "output"])
            .map_or_else(|| compact_json(value), str::to_string),
        Value::Null => String::new(),
        _ => value.to_string(),
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn truncate_utf8(value: &str, max: usize) -> &str {
    if value.len() <= max {
        return value;
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1
    }
    &value[..end]
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

fn valid_native_session_id(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_NATIVE_SESSION_ID_BYTES
        && !value.starts_with('-')
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\'])
        && !value.chars().any(char::is_control)
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn deduplicate_losses(losses: Vec<String>) -> Vec<String> {
    let mut output = Vec::new();
    for loss in losses {
        if !output.contains(&loss) {
            output.push(loss)
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_path_comparison_fails_closed_when_canonicalization_fails() {
        let temp = tempfile::tempdir().unwrap();
        let existing = temp.path().join("existing");
        fs::create_dir(&existing).unwrap();

        assert!(same_path(&existing, &existing));
        assert!(!same_path(&existing, &temp.path().join("missing")));
        assert!(!same_path(
            &temp.path().join("missing-left"),
            &temp.path().join("missing-right")
        ));
    }

    #[tokio::test]
    async fn opencode_candidate_discovery_is_newest_first_and_checkout_scoped() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let other = temp.path().join("other");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&other).unwrap();
        let db_root = temp.path().join("opencode");
        fs::create_dir_all(&db_root).unwrap();
        let connection = Connection::open(db_root.join("opencode.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session( \
                     id TEXT PRIMARY KEY, directory TEXT NOT NULL, time_updated INTEGER NOT NULL);",
            )
            .unwrap();
        for (id, directory, updated) in [
            ("older", &cwd, 100_i64),
            ("newer", &cwd, 200_i64),
            ("unrelated", &other, 300_i64),
        ] {
            connection
                .execute(
                    "INSERT INTO session VALUES (?1, ?2, ?3)",
                    params![id, directory.to_string_lossy(), updated],
                )
                .unwrap();
        }

        let sessions = list_native_sessions(
            ManagedHarness::OpenCode,
            temp.path(),
            &cwd,
            Some(&db_root),
            8,
        )
        .await
        .unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|candidate| candidate.native_session_id.as_str())
                .collect::<Vec<_>>(),
            ["newer", "older"]
        );
        assert!(
            native_session_exists(
                ManagedHarness::OpenCode,
                temp.path(),
                &cwd,
                Some(&db_root),
                "newer"
            )
            .unwrap()
        );
        assert!(
            !native_session_exists(
                ManagedHarness::OpenCode,
                temp.path(),
                &cwd,
                Some(&db_root),
                "missing"
            )
            .unwrap()
        );
    }

    #[test]
    fn incomplete_final_jsonl_record_does_not_advance_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        fs::write(&path, b"{\"type\":\"message\",\"id\":\"one\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n{\"type\":").unwrap();
        let export = export_jsonl(ManagedHarness::Pi, &path, "session", None).unwrap();
        let cursor: FileCursor =
            serde_json::from_str(export.source_cursor.as_deref().unwrap()).unwrap();
        assert_eq!(cursor.offset, 74);
        assert_eq!(export.events.len(), 1);
    }

    #[test]
    fn claude_adapter_excludes_thinking_and_keeps_tools() {
        let value = json!({"type":"assistant","uuid":"record","timestamp":"2026-01-01T00:00:00Z","message":{"role":"assistant","content":[
            {"type":"thinking","thinking":"private"},
            {"type":"tool_use","name":"Read","input":{"file":"README.md"}},
            {"type":"text","text":"Done"}
        ]}});
        let mut events = Vec::new();
        let mut losses = Vec::new();
        parse_claude(&value, "session", "record", &mut events, &mut losses);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, WorkstreamEventKind::ToolCall);
        assert_eq!(events[1].content, "Done");
        assert_eq!(losses.len(), 1);
    }

    #[test]
    fn claude_adapter_excludes_read_back_managed_packets_only_at_the_origin() {
        let value = json!({
            "type":"user",
            "uuid":"record",
            "message":{"role":"user","content":[
                {
                    "type":"tool_result",
                    "content":format!(
                        "\n  {MANAGED_WORKSTREAM_PACKET_MARKER}\n> **ai-memory managed workstream: default**\nprivate packet"
                    )
                },
                {
                    "type":"tool_result",
                    "content":"  > **ai-memory managed workstream: default**\nlegacy packet"
                },
                {
                    "type":"tool_result",
                    "content":format!(
                        "ordinary file output\nmentions {MANAGED_WORKSTREAM_PACKET_MARKER} later"
                    )
                }
            ]}
        });
        let mut events = Vec::new();
        let mut losses = Vec::new();
        parse_claude(&value, "session", "record", &mut events, &mut losses);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, WorkstreamEventKind::ToolResult);
        assert!(events[0].content.starts_with("ordinary file output"));
        assert_eq!(
            losses,
            [
                "Claude managed workstream delivery packets were intentionally excluded",
                "Claude managed workstream delivery packets were intentionally excluded"
            ]
        );
    }

    #[test]
    fn claude_adapter_keeps_compaction_and_excludes_meta_records() {
        let compact = json!({
            "type":"system",
            "subtype":"compact_boundary",
            "uuid":"compact",
            "content":"portable compact summary",
            "isMeta":false
        });
        let meta = json!({
            "type":"user",
            "uuid":"meta",
            "isMeta":true,
            "message":{"role":"user","content":"private harness metadata"}
        });
        let mut events = Vec::new();
        let mut losses = Vec::new();
        parse_claude(&compact, "session", "compact", &mut events, &mut losses);
        parse_claude(&meta, "session", "meta", &mut events, &mut losses);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, WorkstreamEventKind::Compaction);
        assert_eq!(events[0].content, "portable compact summary");
        assert_eq!(
            losses,
            ["Claude synthetic/meta records were intentionally excluded"]
        );
    }

    #[test]
    fn event_ids_are_stable() {
        let mut first = Vec::new();
        let mut second = Vec::new();
        push_event(
            &mut first,
            AgentKind::ClaudeCode,
            "s",
            "r",
            0,
            WorkstreamEventKind::Message,
            Some("user"),
            "hello",
            None,
            json!({}),
        );
        push_event(
            &mut second,
            AgentKind::ClaudeCode,
            "s",
            "r",
            0,
            WorkstreamEventKind::Message,
            Some("user"),
            "hello",
            None,
            json!({}),
        );
        assert_eq!(first[0].event_id, second[0].event_id);
    }

    #[test]
    fn message_adapters_exclude_non_conversation_roles() {
        let claude = json!({
            "type":"user",
            "message":{"role":"system","content":"private Claude instructions"}
        });
        let opencode_message = json!({"role":"system"});
        let opencode_part = json!({"type":"text","text":"private OpenCode instructions"});
        let mut events = Vec::new();
        let mut losses = Vec::new();

        parse_claude(&claude, "session", "claude", &mut events, &mut losses);
        parse_opencode(
            &opencode_message,
            &opencode_part,
            "session",
            "opencode",
            &mut events,
            &mut losses,
        );

        assert!(events.is_empty());
        assert_eq!(losses.len(), 2);
    }

    #[test]
    fn opencode_adapter_reads_sqlite_incrementally_without_writing_it() {
        let home = tempfile::tempdir().unwrap();
        let db = opencode_db(home.path(), None);
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT);\n\
                 CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, \
                                   time_updated INTEGER, data TEXT);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message VALUES ('m1', 's1', ?1)",
                [json!({"role":"user"}).to_string()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO part VALUES ('p1', 'm1', 's1', 1, ?1)",
                [json!({"type":"text","text":"first"}).to_string()],
            )
            .unwrap();

        let first = export_opencode(home.path(), None, "s1", None).unwrap();
        assert_eq!(first.events.len(), 1);
        assert_eq!(first.events[0].content, "first");
        connection
            .execute(
                "INSERT INTO part VALUES ('p2', 'm1', 's1', 2, ?1)",
                [json!({"type":"text","text":"second"}).to_string()],
            )
            .unwrap();
        let second =
            export_opencode(home.path(), None, "s1", first.source_cursor.as_deref()).unwrap();
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].content, "second");
    }

    /// Beta store fixture: `session_v2` + `session_message` with the shapes
    /// observed on beta-18999 (user text, assistant text/tool/reasoning).
    fn opencode2_fixture(home: &Path) {
        let db = opencode_db(home, None);
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session_v2(id TEXT PRIMARY KEY, directory TEXT NOT NULL, \
                                   time_updated INTEGER NOT NULL);\
                 CREATE TABLE session_message(id TEXT PRIMARY KEY, session_id TEXT, type TEXT, \
                                   time_updated INTEGER, data TEXT);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session_v2 VALUES ('s2', ?1, 10)",
                [home.join("repo").to_string_lossy().to_string()],
            )
            .unwrap();
        for (id, updated, kind, data) in [
            (
                "m1",
                1,
                "user",
                json!({"text": "do the thing", "files": []}).to_string(),
            ),
            (
                "m2",
                2,
                "assistant",
                json!({"content": [
                    {"type": "reasoning", "text": "", "state": {}},
                    {"type": "text", "text": "on it"},
                    {"type": "tool", "name": "read", "state": {
                        "status": "completed",
                        "input": {"path": "a.txt"},
                        "content": [{"type": "text", "text": "file bytes"}],
                    }},
                ]})
                .to_string(),
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO session_message VALUES (?1, 's2', ?2, ?3, ?4)",
                    params![id, kind, updated, data],
                )
                .unwrap();
        }
    }

    #[test]
    fn opencode2_adapter_reads_v2_tables_incrementally() {
        let home = tempfile::tempdir().unwrap();
        opencode2_fixture(home.path());

        let first = export_opencode2(home.path(), None, "s2", None).unwrap();
        let contents: Vec<_> = first
            .events
            .iter()
            .map(|event| (event.kind, event.content.as_str()))
            .collect();
        assert!(contents.contains(&(WorkstreamEventKind::Message, "do the thing")));
        assert!(contents.contains(&(WorkstreamEventKind::Message, "on it")));
        assert!(
            contents
                .iter()
                .any(|(kind, content)| *kind == WorkstreamEventKind::ToolCall
                    && content.starts_with("read: "))
        );
        assert!(contents.contains(&(WorkstreamEventKind::ToolResult, "file bytes")));
        assert!(
            first.losses.iter().any(|loss| loss.contains("reasoning")),
            "encrypted reasoning must stay a named loss: {:?}",
            first.losses
        );

        // Incremental cursor: nothing new since the first export.
        let second =
            export_opencode2(home.path(), None, "s2", first.source_cursor.as_deref()).unwrap();
        assert!(second.events.is_empty());
        assert!(second.losses.is_empty());
    }

    #[test]
    fn opencode2_updated_tracks_session_v2_rows() {
        let home = tempfile::tempdir().unwrap();
        opencode2_fixture(home.path());
        assert_eq!(
            opencode2_updated(home.path(), None, "s2").unwrap(),
            Some(10)
        );
        assert_eq!(
            opencode2_updated(home.path(), None, "missing").unwrap(),
            None
        );
    }

    #[test]
    fn opencode2_discovery_ignores_other_checkouts() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let other = temp.path().join("other");
        fs::create_dir_all(&cwd).unwrap();
        let db_root = temp.path().join("opencode");
        fs::create_dir_all(&db_root).unwrap();
        let connection = Connection::open(db_root.join("opencode.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session_v2( \
                     id TEXT PRIMARY KEY, directory TEXT NOT NULL, time_updated INTEGER NOT NULL);",
            )
            .unwrap();
        for (id, directory, updated) in [
            ("older", &cwd, 100_i64),
            ("newer", &cwd, 200_i64),
            ("unrelated", &other, 300_i64),
        ] {
            connection
                .execute(
                    "INSERT INTO session_v2 VALUES (?1, ?2, ?3)",
                    params![id, directory.to_string_lossy(), updated],
                )
                .unwrap();
        }
        let found = discover_opencode2(
            temp.path(),
            Some(&db_root),
            &cwd,
            UNIX_EPOCH + Duration::from_millis(150),
        )
        .unwrap();
        assert_eq!(found.as_deref(), Some("newer"));
    }

}
}
