//! Provider-neutral detection of agent-owned command executions from ACP
//! tool-call lifecycle data.
//!
//! Agents that run commands themselves (rather than through Batey's native
//! `terminal/create`) report them as tool calls with `kind: execute` and
//! structured fields for command, working directory, exit code, and output.
//! The exact field names vary by agent (for example `commandLine` versus
//! `command`, `workingDir` versus `cwd`, `exitCode` versus `exit_code`,
//! `combinedOutput` versus `formatted_output`). This module normalizes those
//! shapes without special-casing any agent name.
//!
//! The rules mirror the frontend `terminal-payload.ts` normalization:
//! - Non-terminal kinds (read, edit, delete, move, search, think, fetch,
//!   switch mode) never become tasks.
//! - A command is required. It comes from structured `raw_input`/`raw_output`
//!   fields first, then from a structured `Terminal:` title prefix. Arbitrary
//!   natural-language text is never inferred as a command.
//! - Exit code, output, and status are optional and may arrive in any order.
//! - Native `Terminal` content that references a Batey-managed terminal is a
//!   duplicate representation, never a new observational task.

use std::path::PathBuf;

use agent_client_protocol_schema::v1::{ToolCallContent, ToolKind};
use serde_json::Value;

use super::TaskState;

const COMMAND_KEYS: &[&str] = &["commandLine", "command_line", "command", "cmd", "exec"];
const WORKDIR_KEYS: &[&str] = &[
    "workingDir",
    "working_dir",
    "workdir",
    "workingDirectory",
    "working_directory",
    "cwd",
];
const EXIT_CODE_KEYS: &[&str] = &[
    "exitCode",
    "exit_code",
    "exitStatus",
    "exit_status",
    "statusCode",
    "status_code",
    "returnCode",
    "return_code",
    // Some agents nest the numeric status under a bare `exit` key (for
    // example inside a `metadata` object). The value must still normalize to
    // an integer; non-numeric `exit` values are ignored.
    "exit",
];
const OUTPUT_KEYS: &[&str] = &[
    "combinedOutput",
    "combined_output",
    "formatted_output",
    "formattedOutput",
    "output",
    "stdout",
    "stderr",
];
const STATUS_KEYS: &[&str] = &["state", "status", "executionState", "execution_state"];
const TERMINAL_ID_KEYS: &[&str] = &["terminal_id", "terminalId", "terminal"];

const GENERIC_TITLES: &[&str] = &[
    "terminal",
    "tool call",
    "execute",
    "command",
    "run command",
    "execute command",
    "bash",
    "sh",
    "zsh",
];

/// Public task id for an observational record namespaced by chat. Agent-owned
/// tool-call ids are only unique within their own ACP session, so two chats
/// can legitimately report the same id (for example `tool-1`). The composite
/// stays stable for the (chat, tool) pair for the tracker's lifetime, which
/// is all the task API needs. Chat ids are server-generated UUIDs without
/// colons, so the encoding is unambiguous. Managed terminal ids never contain
/// a colon and can never collide with this namespace.
pub fn observed_task_id(chat_id: &str, tool_call_id: &str) -> String {
    format!("obs:{chat_id}:{tool_call_id}")
}

/// Normalized observational update extracted from one tool-call event.
#[derive(Debug, Clone)]
pub struct ObservedUpdate {
    pub command: Option<String>,
    pub cwd: Option<PathBuf>,
    pub output: Option<String>,
    pub exit_code: Option<i32>,
    pub state: TaskState,
    /// True when the payload carries enough structured terminal information
    /// to warrant a task record (at minimum a command plus terminal kind or
    /// terminal evidence).
    pub has_terminal_evidence: bool,
}

pub fn tool_kind_str(kind: ToolKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_owned()))
        .unwrap_or_else(|| format!("{kind:?}").to_lowercase())
}

/// True for kinds that must never become terminal tasks: file reads, edits,
/// deletes, moves, searches, web calls, thinking, and mode switches.
/// Only `execute` and `other` (with terminal evidence) may become tasks.
pub fn is_non_terminal_kind(kind_str: &str) -> bool {
    matches!(
        kind_str.to_lowercase().as_str(),
        "read" | "edit" | "delete" | "move" | "search" | "think" | "fetch" | "switch_mode"
    )
}

fn normalize_exit_code_value(val: &Value) -> Option<i32> {
    match val {
        Value::Number(n) => n.as_i64().and_then(|v| i32::try_from(v).ok()).or_else(|| {
            n.as_f64().filter(|f| f.fract() == 0.0).and_then(|f| {
                if f >= i32::MIN as f64 && f <= i32::MAX as f64 {
                    Some(f as i32)
                } else {
                    None
                }
            })
        }),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return None;
            }
            // Accept optional leading sign and digits only.
            let ok = !trimmed.is_empty()
                && trimmed
                    .chars()
                    .enumerate()
                    .all(|(i, c)| c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+')));
            if !ok {
                return None;
            }
            trimmed.parse::<i32>().ok()
        }
        _ => None,
    }
}

fn normalize_output_value(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        _ => serde_json::to_string_pretty(val).unwrap_or_else(|_| val.to_string()),
    }
}

/// Searches one object plus its immediate nested objects for a key. Agents
/// vary in nesting (for example `metadata.exit` versus top-level
/// `exit_code`); one level keeps the search provider-neutral without
/// interpreting arbitrary depth.
fn search_maps(obj: &serde_json::Map<String, Value>) -> Vec<&serde_json::Map<String, Value>> {
    let mut maps = vec![obj];
    for val in obj.values() {
        if let Value::Object(nested) = val {
            maps.push(nested);
        }
    }
    maps
}

fn get_str_key(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    for map in search_maps(obj) {
        for key in keys {
            if let Some(Value::String(s)) = map.get(*key) {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

fn get_exit_code_key(obj: &serde_json::Map<String, Value>) -> Option<i32> {
    for map in search_maps(obj) {
        for key in EXIT_CODE_KEYS {
            if let Some(val) = map.get(*key) {
                if let Some(code) = normalize_exit_code_value(val) {
                    return Some(code);
                }
            }
        }
    }
    None
}

fn get_status_key(obj: &serde_json::Map<String, Value>) -> Option<String> {
    for map in search_maps(obj) {
        for key in STATUS_KEYS {
            if let Some(Value::String(s)) = map.get(*key) {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

/// Extracts the primary output body, merging a separate stderr unless it is
/// already contained in the selected output. Nested objects are searched
/// after top-level keys so `metadata.output` still counts.
fn get_output_from_obj(obj: &serde_json::Map<String, Value>) -> Option<String> {
    let mut selected: Option<(String, String)> = None;
    for map in search_maps(obj) {
        for key in OUTPUT_KEYS {
            if let Some(val) = map.get(*key) {
                if val.is_null() {
                    continue;
                }
                let text = normalize_output_value(val);
                if text.is_empty() {
                    continue;
                }
                selected = Some(((*key).to_string(), text));
                break;
            }
        }
        if selected.is_some() {
            break;
        }
    }
    let (selected_key, mut output) = selected?;
    if let Some(Value::String(stderr)) = obj.get("stderr") {
        if selected_key != "stderr" && !stderr.is_empty() && !output.contains(stderr) {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(stderr);
        }
    } else if let Some(stderr_val) = obj.get("stderr") {
        if selected_key != "stderr" && !stderr_val.is_null() {
            let stderr = normalize_output_value(stderr_val);
            if !stderr.is_empty() && !output.contains(&stderr) {
                if !output.is_empty() && !output.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str(&stderr);
            }
        }
    }
    Some(output)
}

fn clean_title_command(title: Option<&str>) -> Option<String> {
    let title = title?.trim();
    if title.is_empty() {
        return None;
    }
    // Strip structured prefixes like "Terminal: <cmd>". Anything else must
    // not be inferred as a command.
    let stripped = ["terminal:", "run command:", "execute command:", "execute:"]
        .iter()
        .find_map(|prefix| {
            if title.len() >= prefix.len() && title[..prefix.len()].eq_ignore_ascii_case(prefix) {
                Some(title[prefix.len()..].trim())
            } else {
                None
            }
        })
        .unwrap_or(title);
    if stripped.is_empty() {
        return None;
    }
    if GENERIC_TITLES.contains(&stripped.to_lowercase().as_str()) {
        return None;
    }
    // Only accept a title-derived command when it looks structured: either it
    // carried an explicit prefix above, or it is a plausible command line.
    // A bare generic word was already rejected; multi-word or path-like
    // titles are accepted as a Codex-style fallback.
    Some(stripped.to_string())
}

fn as_obj(val: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    val?.as_object()
}

/// Tries to parse a string payload as a JSON object (including markdown-fenced
/// JSON), mirroring the frontend fallback.
fn parse_stringified_obj(s: &str) -> Option<serde_json::Map<String, Value>> {
    let trimmed = s.trim();
    let inner = if trimmed.starts_with("```") && trimmed.ends_with("```") && trimmed.len() >= 6 {
        let inner = &trimmed[3..trimmed.len() - 3];
        match inner.find('\n') {
            Some(pos) => inner[pos + 1..].trim(),
            None => inner.trim(),
        }
    } else {
        trimmed
    };
    if !(inner.starts_with('{') && inner.ends_with('}')) {
        return None;
    }
    serde_json::from_str::<Value>(inner)
        .ok()
        .and_then(|v| v.as_object().cloned())
}

/// Collects candidate objects from raw_input/raw_output, including stringified
/// JSON bodies.
fn candidate_objects(
    raw_input: Option<&Value>,
    raw_output: Option<&Value>,
) -> Vec<serde_json::Map<String, Value>> {
    let mut out = Vec::new();
    for val in [raw_input, raw_output].into_iter().flatten() {
        match val {
            Value::Object(map) => out.push(map.clone()),
            Value::String(s) => {
                if let Some(map) = parse_stringified_obj(s) {
                    out.push(map);
                }
            }
            _ => {}
        }
    }
    out
}

fn map_state_from_strings(
    typed_status: Option<&str>,
    raw_status: Option<&str>,
    exit_code: Option<i32>,
) -> TaskState {
    if let Some(raw) = raw_status.map(|s| s.to_lowercase()) {
        match raw.as_str() {
            "cancelled" | "canceled" | "stopped" | "killed" => return TaskState::Stopped,
            _ => {}
        }
    }
    if let Some(code) = exit_code {
        if code == 0 {
            return TaskState::Completed;
        } else {
            return TaskState::Failed;
        }
    }
    let effective = raw_status
        .map(|s| s.to_lowercase())
        .or_else(|| typed_status.map(|s| s.to_lowercase()));
    match effective.as_deref() {
        Some("in_progress") | Some("running") | Some("pending") | None => TaskState::Running,
        Some("failed") | Some("error") | Some("rejected") => TaskState::Failed,
        Some("cancelled") | Some("canceled") | Some("stopped") | Some("killed") => {
            TaskState::Stopped
        }
        // A terminal status without an exit code still completes the task.
        // Unknown future statuses stay running rather than completing early.
        Some("completed") | Some("complete") | Some("success") | Some("succeeded") | Some("ok")
        | Some("done") => TaskState::Completed,
        Some(_) => TaskState::Running,
    }
}

/// Builds an observational update from full tool-call data. Returns None when
/// the kind is non-terminal or a managed-terminal duplicate is detected by
/// the caller (content check happens outside this pure function).
#[allow(clippy::too_many_arguments)]
pub fn parse_observed_update(
    kind_str: &str,
    title: Option<&str>,
    raw_input: Option<&Value>,
    raw_output: Option<&Value>,
    content_text: Option<&str>,
    typed_status: Option<&str>,
) -> Option<ObservedUpdate> {
    if is_non_terminal_kind(kind_str) {
        return None;
    }
    let kind_lower = kind_str.to_lowercase();
    let is_execute = kind_lower == "execute" || kind_lower == "terminal";

    let objs = candidate_objects(raw_input, raw_output);
    let mut ordered: Vec<&serde_json::Map<String, Value>> = Vec::new();
    if let Some(m) = as_obj(raw_input) {
        ordered.push(m);
    }
    if let Some(m) = as_obj(raw_output) {
        ordered.push(m);
    }
    for m in &objs {
        ordered.push(m);
    }
    let mut output_first: Vec<&serde_json::Map<String, Value>> = Vec::new();
    if let Some(m) = as_obj(raw_output) {
        output_first.push(m);
    }
    if let Some(m) = as_obj(raw_input) {
        output_first.push(m);
    }
    for m in &objs {
        output_first.push(m);
    }

    // Command: structured input first, then structured output (some agents
    // echo it), then a structured title fallback.
    let mut command: Option<String> = None;
    for obj in &ordered {
        if let Some(cmd) = get_str_key(obj, COMMAND_KEYS) {
            command = Some(cmd);
            break;
        }
    }
    if command.is_none() {
        command = clean_title_command(title);
    }

    // Working directory: structured input first, then output.
    let mut cwd: Option<PathBuf> = None;
    for obj in &ordered {
        if let Some(dir) = get_str_key(obj, WORKDIR_KEYS) {
            cwd = Some(PathBuf::from(dir));
            break;
        }
    }

    // Exit code: structured output first, then input.
    let mut exit_code: Option<i32> = None;
    for obj in &output_first {
        if let Some(code) = get_exit_code_key(obj) {
            exit_code = Some(code);
            break;
        }
    }

    // Output: structured output keys first, then content text.
    let mut output: Option<String> = None;
    for obj in &output_first {
        if let Some(text) = get_output_from_obj(obj) {
            if !text.trim().is_empty() {
                output = Some(text);
                break;
            }
        }
    }
    if output.is_none() {
        if let Some(text) = content_text {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                // Content text is only terminal output when the payload
                // already looks terminal; otherwise it is a generic tool
                // result and must not create a task by itself.
                output = Some(trimmed.to_string());
            }
        }
    }
    // A bare string raw_output that is not JSON is also output.
    if output.is_none() {
        for val in [raw_output, raw_input].into_iter().flatten() {
            if let Value::String(s) = val {
                if parse_stringified_obj(s).is_none() && !s.trim().is_empty() {
                    output = Some(s.trim().to_string());
                    break;
                }
            }
        }
    }

    // Raw status from payload status/state keys.
    let mut raw_status: Option<String> = None;
    for obj in &output_first {
        if let Some(s) = get_status_key(obj) {
            raw_status = Some(s);
            break;
        }
    }

    // Terminal evidence: command is required. For execute kinds a command
    // alone suffices (running task with no output yet). For other kinds the
    // payload must also carry exit, specific output, or status evidence so a
    // generic tool with a coincidental "command" field never becomes a task.
    let has_command = command.as_ref().is_some_and(|c| !c.trim().is_empty());
    if !has_command {
        return None;
    }
    let has_exit = exit_code.is_some();
    let has_specific_output = objs.iter().any(|obj| {
        [
            "combinedOutput",
            "combined_output",
            "formatted_output",
            "formattedOutput",
        ]
        .iter()
        .any(|k| obj.get(*k).is_some_and(|v| !v.is_null()))
    });
    let has_any_output = output.as_ref().is_some_and(|o| !o.trim().is_empty());
    let has_workdir = cwd.is_some();
    let has_terminal_evidence = if is_execute {
        true
    } else {
        has_exit || has_specific_output || (has_workdir && has_any_output)
    };
    if !has_terminal_evidence {
        return None;
    }

    let state = map_state_from_strings(typed_status, raw_status.as_deref(), exit_code);
    Some(ObservedUpdate {
        command,
        cwd,
        output,
        exit_code,
        state,
        has_terminal_evidence: true,
    })
}

/// Returns terminal ids embedded in tool-call content.
pub fn terminal_ids_in_content(content: &[ToolCallContent]) -> Vec<String> {
    content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Terminal(t) => Some(t.terminal_id.to_string()),
            _ => None,
        })
        .collect()
}

/// Returns terminal ids referenced by structured raw input/output fields.
pub fn terminal_ids_in_raw(raw: Option<&Value>) -> Vec<String> {
    let Some(obj) = as_obj(raw) else {
        return Vec::new();
    };
    TERMINAL_ID_KEYS
        .iter()
        .filter_map(|k| obj.get(*k).and_then(|v| v.as_str()).map(|s| s.to_string()))
        .filter(|s| !s.trim().is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn non_terminal_kinds_never_become_tasks() {
        for kind in [
            "read",
            "edit",
            "delete",
            "move",
            "search",
            "think",
            "fetch",
            "switch_mode",
        ] {
            let update = parse_observed_update(
                kind,
                Some("Terminal: echo hi"),
                Some(&json!({"command": "echo hi"})),
                Some(&json!({"exit_code": 0, "output": "hi"})),
                None,
                Some("completed"),
            );
            assert!(update.is_none(), "kind {kind} must not become a task");
        }
    }

    #[test]
    fn antigravity_shape_with_aliases_is_recognized() {
        let update = parse_observed_update(
            "execute",
            Some("Terminal: cargo test --workspace"),
            Some(&json!({"commandLine": "cargo test --workspace", "workingDir": "/repo"})),
            Some(&json!({
                "exitCode": 0,
                "combinedOutput": "test result: ok",
                "formatted_output": "test result: ok",
            })),
            None,
            Some("completed"),
        )
        .expect("antigravity shape must be recognized");
        assert_eq!(update.command.as_deref(), Some("cargo test --workspace"));
        assert_eq!(
            update.cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
            Some("/repo".to_string())
        );
        assert_eq!(update.exit_code, Some(0));
        assert_eq!(update.output.as_deref(), Some("test result: ok"));
        assert_eq!(update.state, TaskState::Completed);
    }

    #[test]
    fn codex_shape_uses_title_command_and_exit_code() {
        let update = parse_observed_update(
            "execute",
            Some("Terminal: cargo test"),
            None,
            Some(&json!({"formatted_output": "All tests passed", "exit_code": 0})),
            None,
            Some("completed"),
        )
        .expect("codex shape must be recognized");
        assert_eq!(update.command.as_deref(), Some("cargo test"));
        assert_eq!(update.exit_code, Some(0));
        assert_eq!(update.output.as_deref(), Some("All tests passed"));
        assert_eq!(update.state, TaskState::Completed);
    }

    #[test]
    fn running_command_without_output_is_recognized() {
        let update = parse_observed_update(
            "execute",
            Some("Terminal: npm run dev"),
            Some(&json!({"commandLine": "npm run dev", "workingDir": "/app"})),
            None,
            None,
            Some("in_progress"),
        )
        .expect("running command must be recognized");
        assert_eq!(update.state, TaskState::Running);
        assert!(update.exit_code.is_none());
    }

    #[test]
    fn generic_title_without_command_is_rejected() {
        let update = parse_observed_update(
            "other",
            Some("Terminal"),
            Some(&json!({"workingDir": "/app"})),
            Some(&json!({"output": "hi"})),
            None,
            Some("completed"),
        );
        assert!(update.is_none());
    }

    #[test]
    fn other_kind_needs_terminal_evidence() {
        // Other kind with only a coincidental command field and no exit,
        // output, or status evidence beyond content must not become a task
        // when the command comes from raw but no terminal markers exist?
        // A command alone for Other is not enough.
        let update = parse_observed_update(
            "other",
            None,
            Some(&json!({"command": "echo hi"})),
            None,
            None,
            None,
        );
        assert!(update.is_none());
    }

    #[test]
    fn exit_code_drives_failed_state() {
        let update = parse_observed_update(
            "execute",
            Some("cargo check"),
            Some(&json!({"command": "cargo check"})),
            Some(&json!({"exit_code": 1, "output": "error"})),
            None,
            Some("completed"),
        )
        .unwrap();
        assert_eq!(update.state, TaskState::Failed);
    }

    #[test]
    fn opencode_nested_metadata_exit_is_recognized() {
        let update = parse_observed_update(
            "execute",
            Some("echo hello-from-smoke"),
            Some(&json!({"command": "echo hello-from-smoke", "cwd": "/tmp/work"})),
            Some(&json!({
                "output": "hello-from-smoke\n",
                "metadata": {"output": "hello-from-smoke\n", "exit": 0, "truncated": false},
            })),
            Some("hello-from-smoke\n"),
            Some("completed"),
        )
        .expect("opencode nested shape must be recognized");
        assert_eq!(update.command.as_deref(), Some("echo hello-from-smoke"));
        assert_eq!(update.exit_code, Some(0));
        assert!(update
            .output
            .as_deref()
            .is_some_and(|o| o.contains("hello-from-smoke")));
        assert_eq!(update.state, TaskState::Completed);
    }

    #[test]
    fn cancelled_status_maps_to_stopped() {
        let update = parse_observed_update(
            "execute",
            Some("Terminal: sleep 30"),
            Some(&json!({"command": "sleep 30"})),
            Some(&json!({"status": "cancelled"})),
            None,
            Some("in_progress"),
        )
        .unwrap();
        assert_eq!(update.state, TaskState::Stopped);
    }
}
