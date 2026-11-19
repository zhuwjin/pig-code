//! Trigger constants and canned response scripts served by the mock.

use super::streaming::*;

pub const MOCK_FILE_NAME: &str = "README.mock.md";

pub const MOCK_FILE_CONTENT: &str =
    "# mock file\n\nA known file used by the pig-core mock provider for self-testing.\n";

pub const MOCK_REASONING: &str =
    "The user asked me to read a file and summarize it. Call Read first.";

pub const MOCK_REPLY_MARKER: &str = "MOCK_REPLY_OK";

pub const MOCK_REQUEST_ID: &str = "mock-request-id";

pub const MOCK_TRACE_ID: &str = "mock-trace-id";

pub const MOCK_CF_RAY: &str = "mock-cf-ray";

pub const USAGE_TAIL_TRIGGER: &str = "USAGE_TAIL_SCENARIO";

pub const NATIVE_REASONING_REPLAY_TRIGGER: &str = "NATIVE_REASONING_REPLAY_SCENARIO";

pub const TOOL_ONLY_REPLAY_TRIGGER: &str = "TOOL_ONLY_REPLAY_SCENARIO";

pub const EMPTY_LENGTH_TRIGGER: &str = "EMPTY_LENGTH_SCENARIO";

/// Hosted-search native replay scenarios (Responses/Anthropic)
pub const RESPONSES_WEB_SEARCH_TRIGGER: &str = "RESPONSES_WEB_SEARCH_SCENARIO";

pub const RESPONSES_WEB_SEARCH_TEXT: &str = "Native search answer";

pub const ANTHROPIC_WEB_SEARCH_TRIGGER: &str = "ANTHROPIC_WEB_SEARCH_SCENARIO";

pub const ANTHROPIC_WEB_SEARCH_TEXT: &str = "Native search answer";

/// Anthropic `pause_turn` scenarios. The continuation is detected by the
/// assistant text the previous pause returned (compatible endpoints replay
/// text only), so no native block is needed on the wire.
pub const ANTHROPIC_PAUSE_ONCE_TRIGGER: &str = "ANTHROPIC_PAUSE_ONCE_SCENARIO";

pub const ANTHROPIC_PAUSE_ONCE_CONTENT: &str = "PAUSE_ONCE_CONTENT";

pub const ANTHROPIC_PAUSE_ONCE_DONE: &str = "PAUSE_ONCE_DONE";

pub const ANTHROPIC_PAUSE_TWICE_TRIGGER: &str = "ANTHROPIC_PAUSE_TWICE_SCENARIO";

pub const ANTHROPIC_PAUSE_TWICE_CONTENT_1: &str = "PAUSE_TWICE_CONTENT_1";

pub const ANTHROPIC_PAUSE_TWICE_CONTENT_2: &str = "PAUSE_TWICE_CONTENT_2";

pub const ANTHROPIC_PAUSE_TWICE_DONE: &str = "PAUSE_TWICE_DONE";

pub const ANTHROPIC_PAUSE_ALWAYS_TRIGGER: &str = "ANTHROPIC_PAUSE_ALWAYS_SCENARIO";

pub const ANTHROPIC_PAUSE_ALWAYS_PREFIX: &str = "PAUSE_ALWAYS_CONTENT_";

/// Pause with a local `Read` tool call: the continuation must execute it and
/// close the tool_use/tool_result pair before the next request.
pub const ANTHROPIC_PAUSE_TOOL_TRIGGER: &str = "ANTHROPIC_PAUSE_TOOL_SCENARIO";

pub const ANTHROPIC_PAUSE_TOOL_CONTENT: &str = "PAUSE_TOOL_CONTENT";

pub const ANTHROPIC_PAUSE_TOOL_DONE: &str = "PAUSE_TOOL_DONE";

/// Regression guard: stop_sequence stays an ordinary Stop.
pub const ANTHROPIC_STOP_SEQUENCE_TRIGGER: &str = "ANTHROPIC_STOP_SEQUENCE_SCENARIO";

pub const ANTHROPIC_STOP_SEQUENCE_TEXT: &str = "ANTHROPIC_STOP_SEQUENCE_TEXT";

pub const ANTHROPIC_PAUSE_SOURCE_URL: &str = "https://example.com/pause-source";

/// A pause whose response contains only `server_tool_use` +
/// `web_search_tool_result` (no text/thinking). Compatible endpoints cannot
/// replay the native blocks and must not emit an empty assistant.
pub const ANTHROPIC_PAUSE_SERVER_ONLY_TRIGGER: &str = "ANTHROPIC_PAUSE_SERVER_ONLY_SCENARIO";

pub const ANTHROPIC_PAUSE_SERVER_ONLY_DONE: &str = "PAUSE_SERVER_ONLY_DONE";

/// The same shape, but with a large hosted-search result and a usage total
/// above the compact trigger, for pause-continuation preflight tests.
pub const ANTHROPIC_PAUSE_PRESERVE_TRIGGER: &str = "ANTHROPIC_PAUSE_PRESERVE_SCENARIO";

pub const ANTHROPIC_PAUSE_PRESERVE_DONE: &str = "PAUSE_PRESERVE_DONE";

pub const ANTHROPIC_PAUSE_PRESERVE_USAGE: u64 = 120_000;

/// Non-streaming Chat reply whose content is a text/output_text parts array
pub const SIDECAR_CONTENT_ARRAY_TRIGGER: &str = "SIDECAR_CONTENT_ARRAY_SCENARIO";

pub const SIDECAR_CONTENT_ARRAY_TEXT: &str = "array joined";

/// Non-streaming Chat reply with an empty `tool_calls: []` declaration
pub const SIDECAR_EMPTY_TOOL_CALLS_TRIGGER: &str = "SIDECAR_EMPTY_TOOL_CALLS_SCENARIO";

pub const SIDECAR_EMPTY_TOOL_CALLS_TEXT: &str = "empty tool calls ok";

/// Context-window overflow: HTTP 400 for every format, in-band 200 per format
pub const CONTEXT_OVERFLOW_HTTP_TRIGGER: &str = "CONTEXT_OVERFLOW_HTTP_SCENARIO";

pub const CONTEXT_OVERFLOW_STREAM_TRIGGER: &str = "CONTEXT_OVERFLOW_STREAM_SCENARIO";

pub const CONTEXT_OVERFLOW_MARKER: &str = "This model's maximum context length is 8192 tokens";

/// Kimi/Moonshot and GLM overflow shapes (HTTP 400 and in-band 200)
pub const CONTEXT_OVERFLOW_KIMI_TRIGGER: &str = "CONTEXT_OVERFLOW_KIMI_SCENARIO";

pub const CONTEXT_OVERFLOW_KIMI_STREAM_TRIGGER: &str = "CONTEXT_OVERFLOW_KIMI_STREAM_SCENARIO";

pub const CONTEXT_OVERFLOW_GLM_TRIGGER: &str = "CONTEXT_OVERFLOW_GLM_SCENARIO";

pub const CONTEXT_OVERFLOW_GLM_STREAM_TRIGGER: &str = "CONTEXT_OVERFLOW_GLM_STREAM_SCENARIO";

pub const CONTEXT_OVERFLOW_KIMI_MARKER: &str =
    "Invalid request: Your request exceeded model token limit: 262144 (requested: 269030)";

pub const CONTEXT_OVERFLOW_GLM_MARKER: &str = "Prompt exceeds max length";

/// Chat scenario: step 1 reports usage, step 2 returns a tool call without
/// usage, and step 3 returns text. Used to prove preflight drops a stale anchor.
pub const MISSING_USAGE_AFTER_FIRST_TRIGGER: &str = "MISSING_USAGE_AFTER_FIRST_SCENARIO";

pub const MISSING_USAGE_FILE_A: &str = "missing-usage-a.txt";

pub const MISSING_USAGE_FILE_B: &str = "missing-usage-b.txt";

pub const FINISH_LENGTH_TRIGGER: &str = "FINISH_LENGTH_SCENARIO";

pub const FINISH_CONTENT_FILTER_TRIGGER: &str = "FINISH_CONTENT_FILTER_SCENARIO";

pub const MISSING_FINISH_TRIGGER: &str = "MISSING_FINISH_SCENARIO";

pub const MALFORMED_STREAM_TRIGGER: &str = "MALFORMED_STREAM_SCENARIO";

pub const INCOMPLETE_TOOL_FILE: &str = "incomplete-must-not-execute.txt";

/// Memory-write scenario: the user message is `<trigger> <absolute-path>`;
/// the first call Writes that path with a canned frontmatter fact (ZCode's
/// model-managed memory — the plain Write tool into the memory directory,
/// exercising the out-of-workspace RW-root exemption), then plain text.
pub const MEMORY_WRITE_TRIGGER: &str = "MEMORY_WRITE_SCENARIO";

pub const MEMORY_WRITE_MARKER: &str = "MOCK_MEMORY_WRITE_OK";

/// Canned fact file the memory-write scenario writes (ZCode frontmatter shape)
pub const MOCK_MEMORY_NOTE: &str = "---\nname: mock-note\ndescription: The user prefers concise replies\nmetadata:\n  type: feedback\n---\n\nThe user prefers concise replies.\n";

/// Scenario B: when the user message contains this marker, run the full modification chain Write → Edit → Bash → text.
pub const SCENARIO_B_TRIGGER: &str = "SCENARIO_B";

pub const SCENARIO_B_MARKER: &str = "MOCK_SCENARIO_B_OK";

pub const SCENARIO_B_FILE: &str = "src/hello.txt";

pub const SCENARIO_B_CONTENT: &str = "hello\nline2\nline3\n";

pub const SCENARIO_B_BASH_MARKER: &str = "MOCK_BASH_OK";

/// Scenario B's Bash call: a read-only command (ReadOnly/Auto run it without
/// approval), in a dialect every sandboxed shell can run. `printf` is bash-only and `cmd /C`
/// trips MSYS path conversion, so the child process is `powershell`, which is
/// reachable from both the PowerShell tier and the Git Bash tier (and never
/// appears in the read-only allowlist).
pub fn scenario_b_bash_command() -> String {
    if cfg!(windows) {
        format!("powershell -NoProfile -Command \"Write-Output {SCENARIO_B_BASH_MARKER}\"")
    } else {
        format!("printf '%s\\n' {SCENARIO_B_BASH_MARKER}")
    }
}

/// Thinking ticker demo scenario: when this marker is present, stream
/// **variable-speed** multi-line thinking directly (no tool calls) — slow long
/// lines (to watch tail-pinned horizontal scrolling/fade) → rapid-fire short
/// lines (to watch vertical-scroll queueing/skipping) → steady finish (to
/// watch the line-by-line vertical scroll rhythm) → text containing the
/// marker. Does not use the unified 50ms write loop (each chunk carries its
/// own delay).
pub const SCENARIO_TICKER_TRIGGER: &str = "TICKER_SCENARIO";

pub const TICKER_MARKER: &str = "MOCK_TICKER_DONE";

/// TodoList scenario: when this marker is present, run TodoList write → text (for testing todo persistence).
pub const TODO_SCENARIO_TRIGGER: &str = "TODO_SCENARIO";

pub const TODO_SCENARIO_MARKER: &str = "MOCK_TODO_OK";

pub const TODO_SCENARIO_ITEM: &str = "persistent todo item";

/// AskUserQuestion scenario: marker present and no tool results → an
/// AskUserQuestion call (1 question, 2 options); with tool results → text
/// containing the marker.
pub const SCENARIO_Q_TRIGGER: &str = "SCENARIO_Q";

pub const MOCK_Q_MARKER: &str = "MOCK_QUESTION_OK";

/// Dangerous command scenario: when this marker is present, advance by the
/// number of tool results — 0/1 results both send a dangerous Bash (the second
/// verifies that AlwaysAllow does not remember dangerous commands), ≥2 → text
/// containing the marker. The command is mkfs: it hits the blocklist (the mkfs
/// command slot) yet is harmless to run — macOS has no such command (exit 127),
/// and a no-argument call on Linux only prints usage without touching the disk.
pub const SCENARIO_DANGER_TRIGGER: &str = "DANGER_SCENARIO";

pub const DANGER_COMMAND: &str = "mkfs";

pub const DANGER_MARKER: &str = "MOCK_DANGER_DONE";

/// Subject granularity scenario (AlwaysAllow refinement check): Write a →
/// Write a → Write b → Bash echo → Bash echo → Bash ls → text. The second call
/// with the same subject should not trigger another approval popup.
pub const SCENARIO_SUBJECT_TRIGGER: &str = "SUBJECT_SCENARIO";

pub const SUBJECT_MARKER: &str = "MOCK_SUBJECT_DONE";

pub const SUBJECT_FILE_A: &str = "subject_a.txt";

pub const SUBJECT_FILE_B: &str = "subject_b.txt";

/// Read-only command scenario: Bash ls → text.
pub const SCENARIO_READONLY_TRIGGER: &str = "READONLY_SCENARIO";

/// Out-of-workspace approval scenarios (tests/fs_outside.rs): the message
/// carries the absolute target path as the whitespace-delimited token right
/// after the trigger — tests use forward-slash paths, which survive the raw
/// JSON body without escaping.
pub const SCENARIO_OUTSIDE_READ_TRIGGER: &str = "SCENARIO_OUTSIDE_READ";

pub const SCENARIO_OUTSIDE_WRITE_TRIGGER: &str = "SCENARIO_OUTSIDE_WRITE";

/// Mid-turn variant: TWO outside reads in one turn (round 0 and round 1),
/// done text from round 2 — kept for manual multi-approval checks; each
/// request is approved independently (there is no session-level fs switch)
pub const SCENARIO_OUTSIDE_READ2_TRIGGER: &str = "SCENARIO_READ2_OUTSIDE";

pub const READONLY_MARKER: &str = "MOCK_READONLY_DONE";

/// Slow command scenario (verifies the stop-while-tool-running path): Bash
/// sleep 30 → text finish. Intended for interruption cases; do not use it in
/// non-interrupting cases (it really waits 30s).
pub const SCENARIO_SLOW_TRIGGER: &str = "SLOW_BASH_SCENARIO";

pub const SLOW_MARKER: &str = "MOCK_SLOW_DONE";

pub const SCENARIO_BACKGROUND_TRIGGER: &str = "BACKGROUND_TASK_SCENARIO";
pub const SCENARIO_TASK_WAIT_TRIGGER: &str = "TASK_WAIT_SCENARIO";
pub const SCENARIO_BACKGROUND_FAILURE_TRIGGER: &str = "BACKGROUND_FAILURE_SCENARIO";
pub const BACKGROUND_TASK_OUTPUT: &str = "MOCK_BACKGROUND_TASK_DONE";

/// Plan exit scenario (ExitPlanMode check): 0 results → ExitPlanMode; 1 result
/// containing "Plan approved" (user Allow) → Write plan_exit.txt; otherwise a
/// text finish (a Reject result lacks the approval text → finish directly).
pub const SCENARIO_PLAN_EXIT_TRIGGER: &str = "PLAN_EXIT_SCENARIO";

pub const PLAN_EXIT_MARKER: &str = "MOCK_PLAN_EXIT_DONE";

pub const PLAN_EXIT_FILE: &str = "plan_exit.txt";

/// Plan enter/restore scenario (EnterPlanMode check): EnterPlanMode → Write
/// (should be hard-rejected by Plan) → ExitPlanMode → Write (executed after the
/// original mode is restored) → text.
pub const SCENARIO_PLAN_ENTER_TRIGGER: &str = "PLAN_ENTER_SCENARIO";

pub const PLAN_ENTER_MARKER: &str = "MOCK_PLAN_ENTER_DONE";

pub const PLAN_ENTER_FILE: &str = "plan_enter.txt";

/// Plan file semantics scenario: bare ExitPlanMode (no plan argument → core
/// reads the plan file). After approval (result contains "Plan approved") →
/// Write plan_file_exec.txt to verify execution.
pub const SCENARIO_PLAN_FILE_TRIGGER: &str = "PLAN_FILE_SCENARIO";

pub const PLAN_FILE_EXEC_FILE: &str = "plan_file_exec.txt";

/// Plan write gating scenario: Write to the plans directory (pass-through) → Write a normal file (should be hard-rejected) → text.
pub const SCENARIO_PLAN_WRITE_GATE_TRIGGER: &str = "PLAN_WRITE_GATE_SCENARIO";

/// Plan-mode read-only sandbox scenario: plan mode is on, then a Bash write
/// into the workspace. The tool gate rejects Bash outright, so the marker is
/// never produced and no file appears; the scenario ends with text either way.
pub const SCENARIO_PLAN_BASH_TRIGGER: &str = "PLAN_BASH_SCENARIO";

/// Escalation scenario: Bash with `sandbox_permissions:
/// with_additional_permissions` naming a directory supplied in the user message
/// (the whitespace-delimited token right after the trigger). Round 0 issues the
/// request, later rounds finish with text.
pub const SCENARIO_ESCALATION_TRIGGER: &str = "ESCALATION_SCENARIO";

pub const ESCALATION_MARKER: &str = "MOCK_ESCALATION_DONE";

pub const PLAN_BASH_FILE: &str = "plan_bash_wrote.txt";

pub const PLAN_BASH_MARKER: &str = "MOCK_PLAN_BASH_DONE";

/// A shell write that works in both sandboxed dialects.
pub(super) fn plan_bash_command() -> String {
    if cfg!(windows) {
        format!(
            "powershell -NoProfile -Command \"[System.IO.File]::WriteAllText('{PLAN_BASH_FILE}', 'x')\""
        )
    } else {
        format!("printf x > {PLAN_BASH_FILE}")
    }
}

/// Directory part of a forward-slash path (the test passes `dir/file`).
pub(super) fn escalation_root(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(root, _)| root.to_string())
        .unwrap_or_else(|| path.to_string())
}

pub(super) fn escalation_call(call_id: &str, path: &str) -> Vec<String> {
    tool_call_chunks(
        call_id,
        "ExecCommand",
        &serde_json::json!({
            "command": if cfg!(windows) {
                format!("[System.IO.File]::WriteAllText('{path}', 'x')")
            } else {
                format!("printf x > '{path}'")
            },
            "sandbox_permissions": "with_additional_permissions",
            "additional_permissions": {
                "file_system": { "write": [escalation_root(path)] }
            },
            "justification": "writes a generated artifact outside the workspace"
        })
        .to_string(),
        None,
    )
}

pub(super) fn escalation_response(tool_results: usize, path: &str) -> Vec<String> {
    match tool_results {
        0 => escalation_call("call_esc_1", path),
        _ => vec![
            sse_chunk(serde_json::json!({"content": ESCALATION_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// One command asking for write access to a single **file** outside the
/// workspace. The credential-directory gate distinguishes this from the
/// directory form: a file inside a protected directory is approvable, a
/// directory is not.
pub const CREDENTIAL_FILE_TRIGGER: &str = "CREDENTIAL_FILE_SCENARIO";

pub const CREDENTIAL_FILE_MARKER: &str = "MOCK_CREDENTIAL_FILE_DONE";

pub(super) fn credential_file_call(call_id: &str, path: &str) -> Vec<String> {
    tool_call_chunks(
        call_id,
        "ExecCommand",
        &serde_json::json!({
            "command": if cfg!(windows) {
                format!("[System.IO.File]::WriteAllText('{path}', 'x')")
            } else {
                format!("printf x > '{path}'")
            },
            "sandbox_permissions": "with_additional_permissions",
            "additional_permissions": {
                "file_system": { "write": [path] }
            },
            "justification": "updates one entry in a credential store"
        })
        .to_string(),
        None,
    )
}

pub(super) fn credential_file_response(tool_results: usize, path: &str) -> Vec<String> {
    match tool_results {
        0 => credential_file_call("call_cred_1", path),
        _ => vec![
            sse_chunk(serde_json::json!({"content": CREDENTIAL_FILE_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// One command asking for a **directory** inside a protected credential store.
/// The gate must refuse it outright (no dialog): only single files are
/// approvable there.
pub const CREDENTIAL_DIR_TRIGGER: &str = "CREDENTIAL_DIR_SCENARIO";

pub const CREDENTIAL_DIR_MARKER: &str = "MOCK_CREDENTIAL_DIR_DONE";

pub(super) fn credential_dir_call(call_id: &str, path: &str) -> Vec<String> {
    let directory = path.rsplit_once('/').map(|(root, _)| root).unwrap_or(path);
    tool_call_chunks(
        call_id,
        "ExecCommand",
        &serde_json::json!({
            "command": if cfg!(windows) {
                format!("[System.IO.File]::WriteAllText('{path}', 'x')")
            } else {
                format!("printf x > '{path}'")
            },
            "sandbox_permissions": "with_additional_permissions",
            "additional_permissions": {
                "file_system": { "write": [directory] }
            },
            "justification": "writes one file in a credential directory"
        })
        .to_string(),
        None,
    )
}

pub(super) fn credential_dir_response(tool_results: usize, path: &str) -> Vec<String> {
    match tool_results {
        0 => credential_dir_call("call_creddir_1", path),
        _ => vec![
            sse_chunk(serde_json::json!({"content": CREDENTIAL_DIR_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// A plain `Read` (no request syntax) of a credential file: the gate must detect
/// the target and ask on its own.
pub const CREDENTIAL_AUTO_READ_TRIGGER: &str = "CREDENTIAL_AUTO_READ_SCENARIO";

pub const CREDENTIAL_AUTO_READ_MARKER: &str = "MOCK_CREDENTIAL_AUTO_READ_DONE";

pub(super) fn credential_auto_read_response(tool_results: usize, path: &str) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_autoread_1",
            "Read",
            &serde_json::json!({ "path": path }).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(
                serde_json::json!({"content": CREDENTIAL_AUTO_READ_MARKER}),
                None,
            ),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// One Bash command asking for **read** access to a single credential file: the
/// access actually granted must be read, not write.
pub const CREDENTIAL_READ_TRIGGER: &str = "CREDENTIAL_READ_SCENARIO";

pub const CREDENTIAL_READ_MARKER: &str = "MOCK_CREDENTIAL_READ_DONE";

/// One Bash command asking to widen writes onto a path a deny rule covers: the
/// gate rejects it upfront — an approval could never take effect.
pub const DENIED_WIDENING_TRIGGER: &str = "DENIED_WIDENING_SCENARIO";

pub const DENIED_WIDENING_MARKER: &str = "MOCK_DENIED_WIDENING_DONE";

pub(super) fn denied_widening_response(tool_results: usize, path: &str) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_denied_1",
            "ExecCommand",
            &serde_json::json!({
                "command": if cfg!(windows) {
                    format!("[System.IO.File]::WriteAllText('{path}', 'x')")
                } else {
                    format!("printf x > '{path}'")
                },
                // The root is the path itself, so the denial matches the root.
                "sandbox_permissions": "with_additional_permissions",
                "additional_permissions": {
                    "file_system": { "write": [path] }
                },
                "justification": "writes a denied file"
            })
            .to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": DENIED_WIDENING_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// One Bash command asking to widen reads onto a credential **directory**
/// itself: the listing approval — entry names visible, contents denied.
pub const CREDENTIAL_LISTING_TRIGGER: &str = "CREDENTIAL_LISTING_SCENARIO";

pub const CREDENTIAL_LISTING_MARKER: &str = "MOCK_CREDENTIAL_LISTING_DONE";

pub(super) fn credential_listing_response(tool_results: usize, path: &str) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_credlist_1",
            "ExecCommand",
            &serde_json::json!({
                // `ls -la` is not valid PowerShell; the sandboxed model shell on
                // Windows is PowerShell, where the `ls` alias lists names.
                "command": if cfg!(windows) {
                    format!("ls '{path}'")
                } else {
                    format!("ls -la '{path}'")
                },
                "sandbox_permissions": "with_additional_permissions",
                "additional_permissions": { "file_system": { "read": [path] } },
                "justification": "lists a credential directory"
            })
            .to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(
                serde_json::json!({"content": CREDENTIAL_LISTING_MARKER}),
                None,
            ),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Codex's `require_escalated`: round 0 asks to run one command without the
/// sandbox (the write target follows the trigger in the user message); later
/// rounds finish with text.
pub const SCENARIO_REQUIRE_ESCALATED_TRIGGER: &str = "REQUIRE_ESCALATED_SCENARIO";

pub const REQUIRE_ESCALATED_MARKER: &str = "MOCK_REQUIRE_ESCALATED_DONE";

pub(super) fn require_escalated_response(tool_results: usize, path: &str) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_require_escalated",
            "ExecCommand",
            &serde_json::json!({
                "command": if cfg!(windows) {
                    format!("Set-Content -LiteralPath '{path}' -Value 'escalated'")
                } else {
                    format!("printf escalated > '{path}'")
                },
                "sandbox_permissions": "require_escalated",
                "justification": "writes a report outside the workspace"
            })
            .to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(
                serde_json::json!({"content": REQUIRE_ESCALATED_MARKER}),
                None,
            ),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

pub(super) fn credential_read_response(tool_results: usize, path: &str) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_credread_1",
            "ExecCommand",
            &serde_json::json!({
                "command": if cfg!(windows) {
                    format!("Get-Content -LiteralPath '{path}'")
                } else {
                    format!("cat '{path}'")
                },
                "sandbox_permissions": "with_additional_permissions",
                "additional_permissions": {
                    "file_system": { "read": [path] }
                },
                "justification": "reads one host entry"
            })
            .to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": CREDENTIAL_READ_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// A Write (not Bash) into a protected credential store: the dialog path for the
/// file tools.
pub const CREDENTIAL_WRITE_TRIGGER: &str = "CREDENTIAL_WRITE_SCENARIO";

pub const CREDENTIAL_WRITE_MARKER: &str = "MOCK_CREDENTIAL_WRITE_DONE";

pub(super) fn credential_write_response(tool_results: usize, path: &str) -> Vec<String> {
    match tool_results {
        // The freshness rule requires a Read before overwriting an existing
        // file, and probing the write path must not depend on it.
        0 => tool_call_chunks(
            "call_credwrite_read",
            "Read",
            &serde_json::json!({ "path": path }).to_string(),
            None,
        ),
        1 => tool_call_chunks(
            "call_credwrite_1",
            "Write",
            &serde_json::json!({ "path": path, "content": "ssh-ed25519 AAAA new-key
" })
            .to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(
                serde_json::json!({"content": CREDENTIAL_WRITE_MARKER}),
                None,
            ),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Two commands under the same outside root: a widening offers no remembering
/// tier (only Allow/Reject, Codex parity), so the second command asks again
/// even after the first was approved.
pub const SESSION_GRANT_TRIGGER: &str = "SESSION_GRANT_SCENARIO";

pub const SESSION_GRANT_MARKER: &str = "MOCK_SESSION_GRANT_DONE";

pub(super) fn session_grant_response(tool_results: usize, root: &str) -> Vec<String> {
    match tool_results {
        0 => escalation_call("call_sg_1", &format!("{root}/grant-a.txt")),
        1 => escalation_call("call_sg_2", &format!("{root}/grant-b.txt")),
        _ => vec![
            sse_chunk(serde_json::json!({"content": SESSION_GRANT_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// "Remember to project rules" scenario: Write the same file twice (the second
/// write must run approval-free once the first approval added the rule), then
/// a text finish: Write project_rule.txt v1 → Write project_rule.txt v2 → text.
pub const PROJECT_RULE_TRIGGER: &str = "PROJECT_RULE_SCENARIO";

pub const PROJECT_RULE_MARKER: &str = "MOCK_PROJECT_RULE_DONE";

pub const PROJECT_RULE_FILE: &str = "project_rule.txt";

pub(super) fn project_rule_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 | 1 => tool_call_chunks(
            if tool_results == 0 {
                "call_pr_w1"
            } else {
                "call_pr_w2"
            },
            "Write",
            &serde_json::json!({
                "path": PROJECT_RULE_FILE,
                "content": format!("v{}\n", tool_results + 1)
            })
            .to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": PROJECT_RULE_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

pub(super) fn plan_bash_response(tool_results: usize, bash_command: String) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_pb_1",
            "ExecCommand",
            &serde_json::json!({ "command": bash_command }).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": PLAN_BASH_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Business-error-in-200 scenario (OpenAI path): the SSE stream carries an
/// {"error": ...} data frame instead of failing the HTTP status — the
/// OpenAI-compatible gateway behavior the provider layer must surface as a
/// structured error
pub const BUSINESS_ERROR_TRIGGER: &str = "BUSINESS_ERROR_200";

pub const BUSINESS_ERROR_MARKER: &str = "insufficient balance (mock business error)";

/// Empty-completion scenario (OpenAI path): the FIRST streaming request with
/// this marker gets a 200 stream with no content at all (a bare stop chunk);
/// the silent retry gets a normal text reply with the marker
pub const EMPTY_ONCE_TRIGGER: &str = "EMPTY_ONCE";

pub const EMPTY_ONCE_MARKER: &str = "MOCK_EMPTY_RETRY_OK";

/// Empty-completion retry where the discarded attempt still reports usage:
/// the retried attempt's usage must be the only one counted.
pub const EMPTY_USAGE_ONCE_TRIGGER: &str = "EMPTY_USAGE_ONCE";

pub const EMPTY_USAGE_ONCE_MARKER: &str = "MOCK_EMPTY_USAGE_RETRY_OK";

/// Unknown terminal labels: text (and a tool call) stream normally, then the
/// provider reports a label none of the three formats define. Strict
/// rejection must keep the streamed text and must not execute the call.
pub const UNKNOWN_FINISH_TRIGGER: &str = "UNKNOWN_FINISH_SCENARIO";

pub const UNKNOWN_STOP_TRIGGER: &str = "UNKNOWN_STOP_SCENARIO";

pub const UNKNOWN_TERMINAL_TEXT: &str = "partial answer before unknown terminal";

/// A stream that sends one valid text frame and then stalls without closing.
pub const STALL_STREAM_TRIGGER: &str = "STALL_STREAM_SCENARIO";

pub const STALL_STREAM_TEXT: &str = "partial answer before stall";

/// One-shot requests that stall before headers or in the middle of the body
/// (tests use a short idle deadline or cancellation).
pub const SIDECAR_STALL_HEADERS_TRIGGER: &str = "SIDECAR_STALL_HEADERS";

pub const SIDECAR_STALL_BODY_TRIGGER: &str = "SIDECAR_STALL_BODY";

/// A streaming request rejected with a non-retryable HTTP 400 whose error body
/// then stalls mid-transfer: the live status path must bound the body read like
/// any other response body instead of waiting for the server to close.
pub const ERROR_BODY_STALL_TRIGGER: &str = "ERROR_BODY_STALL_SCENARIO";

/// The same stalled error body, but the complete bytes already written carry a
/// typed context-window overflow. Classification must survive the idle failure.
pub const CONTEXT_OVERFLOW_STALL_TRIGGER: &str = "CONTEXT_OVERFLOW_STALL_SCENARIO";

/// A connectivity probe's non-2xx error body that stalls mid-transfer.
pub const CONN_TEST_ERROR_BODY_STALL_TRIGGER: &str = "CONN_TEST_ERROR_BODY_STALL";

/// One 429 with `Retry-After: 0`, then the normal response (verifies the rate
/// limit branch retries without waiting).
pub const RETRY_429_ONCE_TRIGGER: &str = "RETRY_429_ONCE";

/// Two tool calls in ONE response that reuse the same provider call id. The
/// wire id/tool_result pairing stays faithful; internal item and approval ids
/// must stay distinct per invocation. Serial = approval-gated Write calls
/// (sync points), parallel = read-only Read calls (concurrent group).
pub const DUPLICATE_SERIAL_ID_TRIGGER: &str = "DUPLICATE_SERIAL_TOOL_ID";

pub const DUPLICATE_PARALLEL_ID_TRIGGER: &str = "DUPLICATE_PARALLEL_TOOL_ID";

pub const DUPLICATE_SERIAL_ID_MARKER: &str = "MOCK_DUPLICATE_SERIAL_DONE";

pub const DUPLICATE_PARALLEL_ID_MARKER: &str = "MOCK_DUPLICATE_PARALLEL_DONE";

pub const DUPLICATE_CALL_ID: &str = "call_dup";

/// Plan file semantics scenario: bare ExitPlanMode ({} without a plan argument); after approval, a Write follows.
pub(super) fn plan_file_scenario_response(body: &str, tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks("call_pf_1", "ExitPlanMode", "{}", None),
        1 if body.contains("Plan approved") => tool_call_chunks(
            "call_pf_2",
            "Write",
            &serde_json::json!({"path": PLAN_FILE_EXEC_FILE, "content": "executed\n"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": PLAN_EXIT_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Plan write gating scenario: first write the plans directory (pass-through), then a normal file (should be hard-rejected).
pub(super) fn plan_write_gate_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_pg_1",
            "Write",
            &serde_json::json!({"path": ".pigcode/plans/plan-gate.md", "content": "gate plan\n"})
                .to_string(),
            None,
        ),
        1 => tool_call_chunks(
            "call_pg_2",
            "Write",
            &serde_json::json!({"path": "other.txt", "content": "x\n"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": PLAN_EXIT_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Media tool scenario (ReadMediaFile check): 0 results → ReadMediaFile
/// pic.png → text. For capability gating (with input_image=false the session
/// layer errors out without executing) and the image-into-context pipeline.
pub const SCENARIO_MEDIA_TRIGGER: &str = "MEDIA_SCENARIO";

pub const MEDIA_MARKER: &str = "MOCK_MEDIA_DONE";

/// Subagent scenario (parent side): when this marker is present, send an Agent
/// tool call; the first word after the trigger = behavior token
/// (GREP/WRITE/BASH/LOOP/LONG/BG/BASHBG/STEERBG/BGSTOP/RESUME/RESUME_UNKNOWN/RESUME_RUNNING),
/// optional second word = subagent_type (default: BASH/LOOP/LONG →
/// general-purpose, otherwise explore). Once the Agent result returns, the
/// parent finishes with text.
pub const SUBAGENT_TRIGGER: &str = "SUBAGENT_SCENARIO";

pub const SUBAGENT_PARENT_DONE: &str = "MOCK_SUBAGENT_PARENT_DONE";

/// Marker in the subagent's final conclusion text (should be carried back in the parent-side Agent result)
pub const SUBAGENT_CHILD_DONE: &str = "MOCK_SUBAGENT_CHILD_DONE";

pub const SUBAGENT_NESTED_CHILD_DONE: &str = "MOCK_SUBAGENT_NESTED_CHILD_DONE";
pub const SUBAGENT_GRANDCHILD_DONE: &str = "MOCK_SUBAGENT_GRANDCHILD_DONE";

/// Marker in the subagent's new conclusion after a resume run
pub const SUBAGENT_RESUMED_DONE: &str = "MOCK_SUBAGENT_RESUMED_DONE";

/// Behavior token prefix for subagent prompts (used to recognize child-side requests; spliced into Agent.prompt by the parent side)
pub(super) const SUBAGENT_CHILD_PREFIX: &str = "SUBAGENT_CHILD:";

/// The resume behavior token. Format-specific streaming scenarios key on it so
/// child compaction tests can run on every API format, not only Chat.
pub(super) const SUBAGENT_RESUMED_PROMPT: &str = "SUBAGENT_CHILD:RESUMED";

/// Echo the message count (for testing history rebuilding after resume): triggered when the user message contains ECHO_HISTORY.
/// Step 1 reports usage, step 2 returns a local Read call without usage, and
/// the post-compaction request answers with text.
pub(super) fn missing_usage_after_first_response(body: &str) -> Vec<String> {
    if body.contains("Earlier context compacted") {
        return text_response();
    }
    let tool_results = body.matches("\"role\":\"tool\"").count();
    match tool_results {
        0 => tool_call_chunks(
            "call_missing_1",
            "Read",
            &serde_json::json!({"path": MISSING_USAGE_FILE_A}).to_string(),
            Some((900, 100, 1_000)),
        ),
        1 => tool_call_chunks(
            "call_missing_2",
            "Read",
            &serde_json::json!({"path": MISSING_USAGE_FILE_B}).to_string(),
            None,
        ),
        _ => text_response(),
    }
}

pub(super) fn echo_history_response(body: &str) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let count = parsed["messages"]
        .as_array()
        .map(|msgs| msgs.len())
        .unwrap_or(0);
    let text = format!("HISTORY_COUNT:{count}");
    vec![
        sse_chunk(serde_json::json!({"content": text}), None),
        sse_chunk(serde_json::json!({}), Some("stop")),
    ]
}

/// Scenario B advances by the number of tool results in history: 0→Write,
/// 1→Edit, 2→Bash, ≥3→text. The file parameter avoids multiple self-test
/// sessions mutating the same file and interfering with each other.
pub(super) fn outside_path_after(body: &str, trigger: &str) -> Option<String> {
    // The trigger sits inside the JSON-escaped user message: the path token
    // ends at the closing quote, not only at whitespace
    body.split(trigger)
        .nth(1)?
        .split(|c: char| c.is_whitespace() || c == '"')
        .find(|tok| !tok.is_empty())
        .map(str::to_string)
}

pub(super) fn outside_read_response(path: &str, tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_outside_read",
            "Read",
            &serde_json::json!({"path": path}).to_string(),
            None,
        ),
        _ => outside_text_chunks(&format!("OUTSIDE_READ_DONE {path}")),
    }
}

pub(super) fn outside_read2_response(path: &str, tool_results: usize) -> Vec<String> {
    match tool_results {
        0 | 1 => tool_call_chunks(
            "call_outside_read2",
            "Read",
            &serde_json::json!({"path": path}).to_string(),
            None,
        ),
        _ => outside_text_chunks(&format!("OUTSIDE_READ2_DONE {path}")),
    }
}

pub(super) fn outside_write_response(path: &str, tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_outside_write",
            "Write",
            &serde_json::json!({"path": path, "content": "outside write"}).to_string(),
            None,
        ),
        _ => outside_text_chunks(&format!("OUTSIDE_WRITE_DONE {path}")),
    }
}

pub(super) fn scenario_b_response(tool_results: usize, file: &str) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_b_write",
            "Write",
            &serde_json::json!({"path": file, "content": SCENARIO_B_CONTENT}).to_string(),
            None,
        ),
        1 => tool_call_chunks(
            "call_b_edit",
            "Edit",
            &serde_json::json!({"path": file, "old_string": "line2", "new_string": "LINE2"})
                .to_string(),
            None,
        ),
        2 => tool_call_chunks(
            "call_b_bash",
            "ExecCommand",
            // A normal command; approval semantics are covered by this command
            &serde_json::json!({"command": scenario_b_bash_command()}).to_string(),
            None,
        ),
        _ => {
            let markdown = format!(
                "Done: created `{SCENARIO_B_FILE}`, edited one line, ran echo.\n\n**Result**: {SCENARIO_B_MARKER}\n"
            );
            let chars: Vec<char> = markdown.chars().collect();
            let mut chunks: Vec<String> = chars
                .chunks(9)
                .map(|piece| {
                    let delta: String = piece.iter().collect();
                    sse_chunk(serde_json::json!({"content": delta}), None)
                })
                .collect();
            // The final step carries usage (the real API includes usage at the
            // stream end; the watermark bar/turn stats rely on it)
            chunks.push(format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "chatcmpl-mock",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": "mock-model",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 42, "total_tokens": 142},
                })
            ));
            chunks
        }
    }
}

/// Dangerous command scenario: 0/1 tool results both send a dangerous Bash, ≥2 → text finish.
/// Two calls with the same provider id in one assistant response. After both
/// results arrive, a text marker closes the turn.
pub(super) fn duplicate_tool_id_response(tool_results: usize, serial: bool) -> Vec<String> {
    if tool_results > 0 {
        let marker = if serial {
            DUPLICATE_SERIAL_ID_MARKER
        } else {
            DUPLICATE_PARALLEL_ID_MARKER
        };
        return vec![
            sse_chunk(serde_json::json!({"content": marker}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ];
    }
    let (name, first, second) = if serial {
        (
            "Write",
            serde_json::json!({"path": "dup-a.txt", "content": "alpha\n"}),
            serde_json::json!({"path": "dup-b.txt", "content": "beta\n"}),
        )
    } else {
        (
            "Read",
            serde_json::json!({"path": "dup-a.txt"}),
            serde_json::json!({"path": "dup-b.txt"}),
        )
    };
    vec![
        sse_chunk(
            serde_json::json!({"tool_calls": [
                {"index": 0, "id": DUPLICATE_CALL_ID, "type": "function",
                 "function": {"name": name, "arguments": first.to_string()}},
                {"index": 1, "id": DUPLICATE_CALL_ID, "type": "function",
                 "function": {"name": name, "arguments": second.to_string()}},
            ]}),
            None,
        ),
        sse_chunk(serde_json::json!({}), Some("tool_calls")),
    ]
}

pub(super) fn danger_scenario_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 | 1 => tool_call_chunks(
            if tool_results == 0 {
                "call_danger_1"
            } else {
                "call_danger_2"
            },
            "ExecCommand",
            &serde_json::json!({"command": DANGER_COMMAND}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": DANGER_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Subject granularity scenario: the second call with the same subject does not pop up (Write a twice in a row), a different subject pops up.
pub(super) fn subject_scenario_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 | 1 => tool_call_chunks(
            if tool_results == 0 {
                "call_subj_w1"
            } else {
                "call_subj_w2"
            },
            "Write",
            &serde_json::json!({"path": SUBJECT_FILE_A, "content": format!("v{}\n", tool_results)})
                .to_string(),
            None,
        ),
        2 => tool_call_chunks(
            "call_subj_w3",
            "Write",
            &serde_json::json!({"path": SUBJECT_FILE_B, "content": "b\n"}).to_string(),
            None,
        ),
        3 | 4 => tool_call_chunks(
            if tool_results == 3 {
                "call_subj_e1"
            } else {
                "call_subj_e2"
            },
            "ExecCommand",
            &serde_json::json!({"command": format!("echo SUBJ_{}", tool_results)}).to_string(),
            None,
        ),
        5 => tool_call_chunks(
            "call_subj_ls",
            "ExecCommand",
            &serde_json::json!({"command": "ls"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": SUBJECT_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Read-only command scenario: Bash ls → text.
pub(super) fn readonly_scenario_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_ro_ls",
            "ExecCommand",
            &serde_json::json!({"command": "ls"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": READONLY_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Slow command scenario (verifies the stop-while-tool-running path): Bash
/// sleep 30 → text finish. Intended for interruption cases; do not use it in
/// non-interrupting cases (it really waits 30s).
pub(super) fn slow_bash_scenario_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_slow_bash",
            "ExecCommand",
            &serde_json::json!({"command": "sleep 30"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": SLOW_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

pub(super) fn background_failure_scenario_response(body: &str) -> Vec<String> {
    if tool_answered(body, "call_background_failure") {
        return text_response();
    }
    tool_call_chunks(
        "call_background_failure",
        "ExecCommand",
        &serde_json::json!({"command": "exit 7", "run_in_background": true}).to_string(),
        None,
    )
}

pub(super) fn background_task_scenario_response(body: &str) -> Vec<String> {
    if tool_answered(body, "call_background_task") {
        return text_response();
    }
    tool_call_chunks(
        "call_background_task",
        "ExecCommand",
        &serde_json::json!({
            "command": format!("sleep 0.05; echo {BACKGROUND_TASK_OUTPUT}"),
            "run_in_background": true,
        })
        .to_string(),
        None,
    )
}

pub(super) fn task_wait_scenario_response(body: &str) -> Vec<String> {
    if tool_answered(body, "call_task_wait") {
        return text_response();
    }
    if tool_answered(body, "call_task_wait_background") {
        return tool_call_chunks(
            "call_task_wait",
            "TaskWait",
            &serde_json::json!({"timeout_ms": 30_000}).to_string(),
            None,
        );
    }
    tool_call_chunks(
        "call_task_wait_background",
        "ExecCommand",
        &serde_json::json!({
            "command": format!("sleep 0.05; echo {BACKGROUND_TASK_OUTPUT}"),
            "run_in_background": true,
        })
        .to_string(),
        None,
    )
}

/// Plan exit scenario: advances by tool result count; after Allow (result contains "Plan approved") a Write follows.
pub(super) fn plan_exit_scenario_response(body: &str, tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_pe_1",
            "ExitPlanMode",
            &serde_json::json!({"plan": "Step 1: create plan_exit.txt to verify execution"})
                .to_string(),
            None,
        ),
        1 if body.contains("Plan approved") => tool_call_chunks(
            "call_pe_2",
            "Write",
            &serde_json::json!({"path": PLAN_EXIT_FILE, "content": "executed\n"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": PLAN_EXIT_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Plan enter/restore scenario: advances purely by tool result count (a Write hard-rejected by Plan still counts as a step).
pub(super) fn plan_enter_scenario_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_pn_1",
            "EnterPlanMode",
            &serde_json::json!({"reason": "Large change scope; plan first"}).to_string(),
            None,
        ),
        1 => tool_call_chunks(
            "call_pn_2",
            "Write",
            &serde_json::json!({"path": PLAN_ENTER_FILE, "content": "from-plan\n"}).to_string(),
            None,
        ),
        2 => tool_call_chunks(
            "call_pn_3",
            "ExitPlanMode",
            &serde_json::json!({"plan": "Plan ready"}).to_string(),
            None,
        ),
        3 => tool_call_chunks(
            "call_pn_4",
            "Write",
            &serde_json::json!({"path": PLAN_ENTER_FILE, "content": "executed\n"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": PLAN_ENTER_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Media tool scenario: ReadMediaFile pic.png → text.
pub(super) fn media_scenario_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_media_1",
            "ReadMediaFile",
            &serde_json::json!({"path": "pic.png"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": MEDIA_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Two Reads in one assistant response, followed by text after both results.
pub const PREFIX_PARALLEL_SCENARIO: &str = "PREFIX_PARALLEL_SCENARIO";
pub const STRUCTURED_OUTPUT_TRIGGER: &str = "STRUCTURED_AGENT_OUTPUT";

pub(super) const PREFIX_PARALLEL_MARKER: &str = "MOCK_PREFIX_PARALLEL_DONE";

pub(super) const PARALLEL_IDS_FOR_MOCK: [&str; 2] =
    ["call_prefix_parallel_1", "call_prefix_parallel_2"];

pub(super) fn prefix_parallel_response(tool_results: usize) -> Vec<String> {
    if tool_results > 0 {
        return vec![
            sse_chunk(serde_json::json!({"content": PREFIX_PARALLEL_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ];
    }
    let arguments = serde_json::json!({"path": MOCK_FILE_NAME}).to_string();
    vec![
        sse_chunk(
            serde_json::json!({"tool_calls": [
                {
                    "index": 0,
                    "id": "call_prefix_parallel_1",
                    "function": {"name": "Read", "arguments": &arguments},
                },
                {
                    "index": 1,
                    "id": "call_prefix_parallel_2",
                    "function": {"name": "Read", "arguments": &arguments},
                },
            ]}),
            None,
        ),
        sse_chunk(serde_json::json!({}), Some("tool_calls")),
    ]
}

pub const SCENARIO_SLOW_FOLLOWUP_TRIGGER: &str = "SLOW_WITH_UNEXECUTED_FOLLOWUP";

pub const SCENARIO_PARALLEL_SLOW_FOLLOWUP_TRIGGER: &str = "PARALLEL_SLOW_WITH_UNEXECUTED_FOLLOWUP";

pub const PARALLEL_SLOW_MARKER: &str = "MOCK_PARALLEL_SLOW_DONE";

pub(super) fn slow_followup_response() -> Vec<String> {
    let write_arguments = serde_json::json!({
        "path": "cancel-followup-must-not-exist.txt",
        "content": "must not execute",
    })
    .to_string();
    vec![
        sse_chunk(
            serde_json::json!({"tool_calls": [
                {
                    "index": 0,
                    "id": "call_slow_first",
                    "function": {"name": "ExecCommand", "arguments": "{\"command\":\"sleep 30\"}"},
                },
                {
                    "index": 1,
                    "id": "call_unexecuted_write",
                    "function": {"name": "Write", "arguments": write_arguments},
                },
            ]}),
            None,
        ),
        sse_chunk(serde_json::json!({}), Some("tool_calls")),
    ]
}

fn non_tool_user_text(message: &serde_json::Value) -> Option<String> {
    if message["role"].as_str() != Some("user") {
        return None;
    }
    let content = &message["content"];
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let text = content
        .as_array()?
        .iter()
        .filter(|block| matches!(block["type"].as_str(), Some("text" | "input_text")))
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

/// Latest non-tool user text in Chat, Responses, or Anthropic requests.
pub(super) fn last_user_text(body: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    parsed
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .or_else(|| parsed.get("input").and_then(serde_json::Value::as_array))
        .and_then(|messages| messages.iter().rev().find_map(non_tool_user_text))
        .unwrap_or_default()
}

pub(super) fn tool_answered(body: &str, call_id: &str) -> bool {
    let Ok(request) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let message_answered = request
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                (message["role"].as_str() == Some("tool")
                    && message["tool_call_id"].as_str() == Some(call_id))
                    || message["content"].as_array().is_some_and(|blocks| {
                        blocks.iter().any(|block| {
                            block["type"].as_str() == Some("tool_result")
                                && block["tool_use_id"].as_str() == Some(call_id)
                        })
                    })
            })
        });
    message_answered
        || request
            .get("input")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item["type"].as_str() == Some("function_call_output")
                        && item["call_id"].as_str() == Some(call_id)
                })
            })
}

/// Extract "agent_id: a..."/"task_id: b..." from the request body (lines in Agent result templates/background receipts)
pub(super) fn extract_marker_id(body: &str, marker: &str) -> Option<String> {
    let rest = body.split(marker).nth(1)?;
    let id: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    (!id.is_empty()).then_some(id)
}

pub(super) fn subagent_parent_done() -> Vec<String> {
    vec![
        sse_chunk(serde_json::json!({"content": SUBAGENT_PARENT_DONE}), None),
        sse_chunk(serde_json::json!({}), Some("stop")),
    ]
}

pub(super) fn subagent_agent_call(call_id: &str, args: serde_json::Value) -> Vec<String> {
    tool_call_chunks(call_id, "Agent", &args.to_string(), None)
}

pub(super) fn child_prompt(behavior: &str) -> String {
    format!("{SUBAGENT_CHILD_PREFIX}{behavior} read {MOCK_FILE_NAME} and summarize")
}

/// Subagent scenario (parent side): advances the state machine by behavior
/// token —
/// Normal tokens: call_agent_1 (foreground) → result returns → text finish;
/// BG/BASHBG: call_agent_bg (run_in_background) → running receipt → text finish;
/// SWARMBG: call_swarm_bg (AgentSwarm run_in_background, 2 items) → receipt →
/// text finish;
/// BGSTOP: background LOOP → running receipt → TaskStop → text finish;
/// RESUME: first message foreground GREP (call_agent_1), second message
/// resumes the original id (call_agent_2);
/// RESUME_UNKNOWN: resume a nonexistent id directly (call_agent_2);
/// RESUME_RUNNING: first message background LOOP (call_agent_bg), second
/// message resumes the same id (call_agent_2, should hit the conflict).
pub(super) fn subagent_parent_response(body: &str) -> Vec<String> {
    let user_text = last_user_text(body);
    // Wake turn: the last user message is the synthetic <task-notification>
    // message (no trigger) → finish directly
    if !user_text.contains(SUBAGENT_TRIGGER) {
        return subagent_parent_done();
    }
    let mut tokens = user_text
        .split(SUBAGENT_TRIGGER)
        .nth(1)
        .unwrap_or("")
        .split_whitespace();
    let behavior = tokens.next().unwrap_or("GREP");
    let profile_arg = tokens.next();
    match behavior {
        "SWARMBG" => {
            if tool_answered(body, "call_swarm_bg") {
                return subagent_parent_done();
            }
            tool_call_chunks(
                "call_swarm_bg",
                "AgentSwarm",
                &serde_json::json!({
                    "prompt_template": format!("{SUBAGENT_CHILD_PREFIX}GREP handle {{{{item}}}}"),
                    "items": ["item-a", "item-b"],
                    "run_in_background": true,
                })
                .to_string(),
                None,
            )
        }
        "BG" | "BASHBG" | "WRITEBG" => {
            if tool_answered(body, "call_agent_bg") {
                return subagent_parent_done();
            }
            let (child, profile) = match behavior {
                "BG" => ("GREP", "explore"),
                "WRITEBG" => ("WRITE", "general-purpose"),
                _ => ("BASH", "general-purpose"),
            };
            subagent_agent_call(
                "call_agent_bg",
                serde_json::json!({
                    "description": "subagent selftest delegation",
                    "prompt": child_prompt(child),
                    "subagent_type": profile_arg.unwrap_or(profile),
                    "run_in_background": true,
                }),
            )
        }
        "STEERBG" => {
            if tool_answered(body, "call_steer_send") {
                return subagent_parent_done();
            }
            if tool_answered(body, "call_agent_bg") {
                // The background LOOP child is running: steer it via
                // SendMessage (agent_id parsed from the running receipt in
                // the conversation)
                let agent_id = extract_marker_id(body, "agent_id: ").unwrap_or_default();
                return tool_call_chunks(
                    "call_steer_send",
                    "SendMessage",
                    &serde_json::json!({
                        "agent_id": agent_id,
                        "message": "STEER_MARKER_9: focus on the mock file's title line"
                    })
                    .to_string(),
                    None,
                );
            }
            subagent_agent_call(
                "call_agent_bg",
                serde_json::json!({
                    "description": "subagent steer selftest",
                    "prompt": child_prompt("LOOP"),
                    "subagent_type": "general-purpose",
                    "run_in_background": true,
                }),
            )
        }
        "BGSTOP" => {
            if tool_answered(body, "call_taskstop") {
                return subagent_parent_done();
            }
            if tool_answered(body, "call_agent_bg") {
                // The background subagent is already running (LOOP never stops): send TaskStop to stop it
                let task_id =
                    extract_marker_id(body, "task_id: ").unwrap_or_else(|| "b1".to_string());
                return tool_call_chunks(
                    "call_taskstop",
                    "TaskStop",
                    &serde_json::json!({"task_id": task_id}).to_string(),
                    None,
                );
            }
            subagent_agent_call(
                "call_agent_bg",
                serde_json::json!({
                    "description": "subagent selftest delegation",
                    "prompt": child_prompt("LOOP"),
                    "subagent_type": profile_arg.unwrap_or("explore"),
                    "run_in_background": true,
                }),
            )
        }
        "RESUME" => {
            // BGSTOP creates a stopped child; the next RESUME is its first resume.
            let resume_after_stop = body.contains(&format!("{SUBAGENT_TRIGGER} BGSTOP"));
            // Trigger occurrence count = messages already sent: the 1st starts
            // a new subagent, the 2nd resumes the original id, the 3rd covers
            // a resume retried after a failed write.
            if !resume_after_stop && body.matches(SUBAGENT_TRIGGER).count() >= 3 {
                if tool_answered(body, "call_agent_3") {
                    return subagent_parent_done();
                }
                let agent_id = extract_marker_id(body, "agent_id: ")
                    .expect("resume scenario: agent_id should be in history");
                return subagent_agent_call(
                    "call_agent_3",
                    serde_json::json!({
                        "description": "subagent resume retry",
                        "prompt": child_prompt("RESUMED3"),
                        "resume": agent_id,
                    }),
                );
            }
            if resume_after_stop || body.matches(SUBAGENT_TRIGGER).count() >= 2 {
                if tool_answered(body, "call_agent_2") {
                    return subagent_parent_done();
                }
                let agent_id = extract_marker_id(body, "agent_id: ")
                    .expect("resume scenario: agent_id should be in history");
                return subagent_agent_call(
                    "call_agent_2",
                    serde_json::json!({
                        "description": "subagent resume",
                        "prompt": child_prompt("RESUMED"),
                        "resume": agent_id,
                    }),
                );
            }
            if tool_answered(body, "call_agent_1") {
                return subagent_parent_done();
            }
            subagent_agent_call(
                "call_agent_1",
                serde_json::json!({
                    "description": "subagent selftest delegation",
                    "prompt": child_prompt("GREP"),
                    "subagent_type": profile_arg.unwrap_or("explore"),
                }),
            )
        }
        "RESUME_UNKNOWN" => {
            if tool_answered(body, "call_agent_2") {
                return subagent_parent_done();
            }
            subagent_agent_call(
                "call_agent_2",
                serde_json::json!({
                    "description": "resume nonexistent subagent",
                    "prompt": child_prompt("GREP"),
                    "resume": "a0-999",
                }),
            )
        }
        "RESUME_RUNNING" => {
            if body.matches(SUBAGENT_TRIGGER).count() >= 2 {
                if tool_answered(body, "call_agent_2") {
                    return subagent_parent_done();
                }
                let agent_id = extract_marker_id(body, "agent_id: ")
                    .expect("resume scenario: agent_id should be in history");
                return subagent_agent_call(
                    "call_agent_2",
                    serde_json::json!({
                        "description": "resume running subagent",
                        "prompt": child_prompt("GREP"),
                        "resume": agent_id,
                    }),
                );
            }
            if tool_answered(body, "call_agent_bg") {
                return subagent_parent_done();
            }
            subagent_agent_call(
                "call_agent_bg",
                serde_json::json!({
                    "description": "subagent selftest delegation",
                    "prompt": child_prompt("LOOP"),
                    "subagent_type": profile_arg.unwrap_or("explore"),
                    "run_in_background": true,
                }),
            )
        }
        "RESUME_NESTED" => {
            if tool_answered(body, "call_agent_2") {
                return subagent_parent_done();
            }
            subagent_agent_call(
                "call_agent_2",
                serde_json::json!({
                    "description": "resume stopped nested grandchild",
                    "prompt": child_prompt("RESUMED"),
                    "resume": profile_arg.expect("RESUME_NESTED needs the grandchild agent_id"),
                }),
            )
        }
        _ => {
            if tool_answered(body, "call_agent_1") {
                return subagent_parent_done();
            }
            let default_type = match behavior {
                "BASH" | "LOOP" | "LONG" | "NESTED" | "NESTED_SWARM" | "NESTED_BG"
                | "NESTED_SWARM_BG" | "NESTED_BG_LOOP" | "NESTED_BG_WAIT" | "NESTED_BG_STOP"
                | "NESTED_BG_STEER" => "general-purpose",
                _ => "explore",
            };
            subagent_agent_call(
                "call_agent_1",
                serde_json::json!({
                    "description": "subagent selftest delegation",
                    "prompt": child_prompt(behavior),
                    "subagent_type": profile_arg.unwrap_or(default_type),
                }),
            )
        }
    }
}

/// Subagent scenario (child side): acts by the behavior token in the prompt
/// (the last user message) — with 0 tool results it emits a tool call (LOOP
/// always emits tool calls, forcing the parent to finish via max_turns; LONG
/// replies directly with a 33K-char long text; RESUMED replies directly with
/// the resume conclusion), otherwise it replies with the marker-bearing
/// conclusion text.
pub(super) fn subagent_child_response(body: &str, tool_results: usize) -> Vec<String> {
    let user_text = last_user_text(body);
    let behavior_source = if user_text.contains(SUBAGENT_CHILD_PREFIX) {
        &user_text
    } else {
        body
    };
    let behavior = behavior_source
        .split(SUBAGENT_CHILD_PREFIX)
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or("");
    let done_text = |text: String| {
        vec![
            sse_chunk(serde_json::json!({"content": text}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ]
    };
    match behavior {
        "NESTED_SWARM_BG" if !tool_answered(body, "call_nested_swarm_bg") => tool_call_chunks(
            "call_nested_swarm_bg",
            "AgentSwarm",
            &serde_json::json!({
                "prompt_template": format!("{SUBAGENT_CHILD_PREFIX}GRANDCHILD handle {{{{item}}}}"),
                "items": ["nested-bg-a", "nested-bg-b"],
                "subagent_type": "general-purpose",
                "run_in_background": true,
            })
            .to_string(),
            None,
        ),
        "NESTED_SWARM_BG" => done_text(format!(
            "Nested background swarm dispatched. {SUBAGENT_NESTED_CHILD_DONE}"
        )),
        "NESTED_BG" | "NESTED_BG_LOOP" | "NESTED_BG_WAIT" | "NESTED_BG_STOP"
        | "NESTED_BG_STEER"
            if !tool_answered(body, "call_nested_agent_bg") =>
        {
            let child_behavior = if matches!(behavior, "NESTED_BG_LOOP" | "NESTED_BG_STOP") {
                "LOOP"
            } else {
                "GRANDCHILD"
            };
            subagent_agent_call(
                "call_nested_agent_bg",
                serde_json::json!({
                    "description": "nested background grandchild",
                    "prompt": format!("{SUBAGENT_CHILD_PREFIX}{child_behavior} finish the nested background task"),
                    "subagent_type": "general-purpose",
                    "run_in_background": true,
                }),
            )
        }
        "NESTED_BG_STEER" if !tool_answered(body, "call_nested_pause") => tool_call_chunks(
            "call_nested_pause",
            "ExecCommand",
            &serde_json::json!({"command": "sleep 0.2"}).to_string(),
            None,
        ),
        "NESTED_BG_STOP" if !tool_answered(body, "call_nested_stop") => {
            let task_id = extract_marker_id(body, "task_id: ")
                .expect("nested background receipt should contain task_id");
            tool_call_chunks(
                "call_nested_stop",
                "TaskStop",
                &serde_json::json!({"task_id": task_id}).to_string(),
                None,
            )
        }
        "NESTED_BG_WAIT" if !tool_answered(body, "call_nested_wait") => {
            let task_id = extract_marker_id(body, "task_id: ")
                .expect("nested background receipt should contain task_id");
            tool_call_chunks(
                "call_nested_wait",
                "TaskWait",
                &serde_json::json!({"task_ids": [task_id], "timeout_ms": 10_000}).to_string(),
                None,
            )
        }
        "NESTED_BG" | "NESTED_BG_LOOP" | "NESTED_BG_WAIT" | "NESTED_BG_STOP"
        | "NESTED_BG_STEER" => done_text(format!(
            "Nested background child continued. {SUBAGENT_NESTED_CHILD_DONE}"
        )),
        "NESTED_SWARM" if !tool_answered(body, "call_nested_swarm") => tool_call_chunks(
            "call_nested_swarm",
            "AgentSwarm",
            &serde_json::json!({
                "prompt_template": format!("{SUBAGENT_CHILD_PREFIX}GRANDCHILD handle {{{{item}}}}"),
                "items": ["nested-a", "nested-b"],
                "subagent_type": "general-purpose",
            })
            .to_string(),
            None,
        ),
        "NESTED_SWARM" => done_text(format!(
            "Nested swarm done. {SUBAGENT_NESTED_CHILD_DONE} {SUBAGENT_GRANDCHILD_DONE}"
        )),
        "NESTED" if !tool_answered(body, "call_nested_agent") => subagent_agent_call(
            "call_nested_agent",
            serde_json::json!({
                "description": "nested grandchild delegation",
                "prompt": format!("{SUBAGENT_CHILD_PREFIX}GRANDCHILD finish the nested task"),
                "subagent_type": "general-purpose",
            }),
        ),
        "NESTED" => done_text(format!(
            "Nested child done. {SUBAGENT_NESTED_CHILD_DONE} {SUBAGENT_GRANDCHILD_DONE}"
        )),
        "GRANDCHILD" => done_text(format!("Grandchild done. {SUBAGENT_GRANDCHILD_DONE}")),
        "RESUMED" => done_text(format!(
            "Subagent resumed: task done. {SUBAGENT_RESUMED_DONE}"
        )),
        "LONG" => {
            // 33K-char long text (a single repeated CJK char pads the body):
            // verifies the parent's 32K result budget truncation + full-text
            // persistence. 2000 chars per chunk (the mock sleeps 50ms per
            // chunk, so the chunk count must stay low)
            let text = format!("Subagent long result start. {}", "密".repeat(33_000));
            let chars: Vec<char> = text.chars().collect();
            let mut chunks: Vec<String> = chars
                .chunks(2000)
                .map(|piece| {
                    let delta: String = piece.iter().collect();
                    sse_chunk(serde_json::json!({"content": delta}), None)
                })
                .collect();
            chunks.push(sse_chunk(serde_json::json!({}), Some("stop")));
            chunks
        }
        "LOOP" => tool_call_chunks(
            &format!("call_child_loop_{tool_results}"),
            "Grep",
            &serde_json::json!({"pattern": "mock"}).to_string(),
            None,
        ),
        _ if tool_results == 0 => match behavior {
            "WRITE" => tool_call_chunks(
                "call_child_write",
                "Write",
                &serde_json::json!({"path": "child_write.txt", "content": "x\n"}).to_string(),
                None,
            ),
            "BASH" => tool_call_chunks(
                "call_child_bash",
                "ExecCommand",
                &serde_json::json!({"command": "echo x > child_bash.txt"}).to_string(),
                None,
            ),
            _ => tool_call_chunks(
                "call_child_grep",
                "Grep",
                &serde_json::json!({"pattern": "mock"}).to_string(),
                None,
            ),
        },
        _ => done_text(format!(
            "Subagent done: task complete. {SUBAGENT_CHILD_DONE}"
        )),
    }
}

/// Memory-write scenario: no Write of the target yet → Write it with the
/// canned fact; already executed → text finish (history detection like the
/// TodoList scenario, keyed on the canned file name)
pub(super) fn memory_write_response(body: &str, tool_results: usize) -> Vec<String> {
    if tool_results > 0 {
        return vec![
            sse_chunk(serde_json::json!({"content": MEMORY_WRITE_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ];
    }
    // The target path rides the user message: "<trigger> <absolute-path>".
    // Reuse outside_path_after's tokenization (stop at whitespace or the JSON
    // string's closing quote): a Windows path's separators arrive as escaped
    // `\\` pairs inside the body, so stopping at a raw backslash would cut the
    // path at the first separator; resolve_core canonicalizes the parent, so
    // the doubled separators are harmless downstream
    let path = outside_path_after(body, MEMORY_WRITE_TRIGGER)
        .unwrap_or_else(|| "memory-untitled.md".to_string());
    tool_call_chunks(
        "call_memw_1",
        "Write",
        &serde_json::json!({"path": path, "content": MOCK_MEMORY_NOTE}).to_string(),
        None,
    )
}

/// TodoList scenario: no TodoList call in history yet → write one; already
/// executed → text finish. Cannot count global tool results: both the request
/// body's tools declaration and history messages interfere, so directly parse
/// whether a TodoList call appears in messages.
pub(super) fn todo_scenario_response(body: &str) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let called = parsed["messages"].as_array().is_some_and(|msgs| {
        msgs.iter().any(|m| {
            m["tool_calls"].as_array().is_some_and(|calls| {
                calls
                    .iter()
                    .any(|c| c["function"]["name"].as_str() == Some("TodoList"))
            })
        })
    });
    if called {
        vec![
            sse_chunk(serde_json::json!({"content": TODO_SCENARIO_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ]
    } else {
        tool_call_chunks(
            "call_todo_1",
            "TodoList",
            &serde_json::json!({"todos": [
                {"content": format!("{TODO_SCENARIO_ITEM} 1"), "status": "done"},
                {"content": format!("{TODO_SCENARIO_ITEM} 2"), "status": "in_progress"}
            ]})
            .to_string(),
            None,
        )
    }
}

/// AskUserQuestion scenario: no AskUserQuestion call in history yet → ask;
/// already executed → text finish. (Same as the TodoList scenario: scan
/// messages directly; cannot count global tool results)
pub(super) fn question_scenario_response(body: &str) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let called = parsed["messages"].as_array().is_some_and(|msgs| {
        msgs.iter().any(|m| {
            m["tool_calls"].as_array().is_some_and(|calls| {
                calls
                    .iter()
                    .any(|c| c["function"]["name"].as_str() == Some("AskUserQuestion"))
            })
        })
    });
    if called {
        vec![
            sse_chunk(serde_json::json!({"content": MOCK_Q_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ]
    } else {
        tool_call_chunks(
            "call_q_1",
            "AskUserQuestion",
            &serde_json::json!({"questions": [
                {
                    "question": "Choose an implementation approach",
                    "header": "Approach",
                    "options": [
                        {"label": "Option A", "description": "Simple and direct"},
                        {"label": "Option B", "description": "More complete but complex"}
                    ]
                },
                {
                    "question": "Should tests run?",
                    "header": "Tests",
                    "options": [
                        {"label": "Yes", "description": "Run after changes"},
                        {"label": "No", "description": "Skip for now"}
                    ]
                }
            ]})
            .to_string(),
            None,
        )
    }
}

pub(super) fn text_response() -> Vec<String> {
    let markdown = format!(
        "## File summary\n\nContents of `{MOCK_FILE_NAME}`:\n\n```text\nA known file used by the pig-core mock provider for self-testing.\n```\n\n**Conclusion**: {MOCK_REPLY_MARKER}\n"
    );
    let mut chunks = vec![sse_chunk(
        serde_json::json!({"reasoning_content": "The tool returned the file content; composing the answer."}),
        None,
    )];
    let chars: Vec<char> = markdown.chars().collect();
    for piece in chars.chunks(9) {
        let delta: String = piece.iter().collect();
        chunks.push(sse_chunk(serde_json::json!({"content": delta}), None));
    }
    chunks.push(format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "mock-model",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 42, "total_tokens": 142},
        })
    ));
    chunks
}

pub const SCENARIO_C_TRIGGER: &str = "SCENARIO_C";

pub const PLAN_MARKER: &str = "MOCK_PLAN_OK";

pub const SUMMARY_MARKER: &str = "MOCK_SUMMARY_OK";

/// Mock title for the session auto-naming sidecar (for selftest assertions)
pub const MOCK_TITLE: &str = "auto-title selftest";

/// Streaming-args scenario (tool_input live rendering, all three formats):
/// 0 results → a Write whose arguments arrive as per-line raw-JSON fragments
/// (exercising the newline batching and the pending card); 1 result → a text
/// reply with the marker
pub const STREAM_ARGS_TRIGGER: &str = "STREAM_ARGS_SCENARIO";

pub const STREAM_ARGS_MARKER: &str = "MOCK_STREAM_ARGS_DONE";

pub const STREAM_ARGS_FILE: &str = "stream_args.txt";

pub const STREAM_ARGS_CONTENT: &str = "alpha line\nbeta line\ngamma line 中文\n";

pub(super) fn stream_args_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => stream_args_chat_chunks(),
        _ => vec![
            sse_chunk(
                serde_json::json!({"content": format!("Streamed write finished: {STREAM_ARGS_MARKER}")}),
                None,
            ),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

// ---------------- Streaming-all-tools demo scenario ----------------

/// Streaming-all-tools demo (STREAM_ALL_SCENARIO, all three formats): the
/// five demo calls stream ONE PER RESPONSE (tool_results counts how many have
/// executed → which call streams now), so every card visibly goes pending →
/// running → settled before the next call's arguments start — Write line by
/// line (each content line its own fragment), Edit field by field
/// (old_string's red rows land before new_string's green ones), then Bash
/// (multi-line command growing in the summary row), Grep and Read. Edit/Grep/
/// Read all target the file Write created two responses earlier.
/// tool_results ≥ 5 → a text reply with the marker.
pub const STREAM_ALL_TRIGGER: &str = "STREAM_ALL_SCENARIO";

pub const STREAM_ALL_MARKER: &str = "MOCK_STREAM_ALL_DONE";

pub const STREAM_ALL_FILE: &str = "stream_demo.txt";

/// Scenario C: plan mode (kimi file semantics) — Write a plan file →
/// ExitPlanMode → after approval, the scenario B tool chain follows (same as
/// the live full pipeline: Write card → plan line → approval panel → work
/// starts)
pub(super) fn scenario_c_response(tool_results: usize) -> Vec<String> {
    let plan = format!(
        "## Execution plan\n\n1. Create `src/hello.txt` with three lines\n2. Uppercase the second line\n3. Run echo to verify\n\n**Plan ready**: {PLAN_MARKER}\n"
    );
    match tool_results {
        0 => tool_call_chunks(
            "call_c_1",
            "Write",
            &serde_json::json!({"path": ".pigcode/plans/plan-mock.md", "content": plan})
                .to_string(),
            None,
        ),
        1 => tool_call_chunks(
            "call_c_2",
            "ExitPlanMode",
            &serde_json::json!({"plan": plan}).to_string(),
            None,
        ),
        _ => scenario_b_response(tool_results - 2, "src/hello_plan.txt"),
    }
}

/// ECHO_USAGE <n>: text reply + the given total_tokens (for testing auto compact triggering).
pub(super) fn echo_usage_response(body: &str) -> Vec<String> {
    let usage: u64 = {
        let marker = "ECHO_USAGE";
        let pos = body.find(marker).map(|p| p + marker.len()).unwrap_or(0);
        body[pos..]
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    };
    vec![
        sse_chunk(serde_json::json!({"content": "usage noted"}), None),
        format!(
            "data: {}

",
            serde_json::json!({
                "id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": 0, "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": usage},
            })
        ),
    ]
}
