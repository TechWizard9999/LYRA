//! Claude Code adapter: `claude -p` with `--output-format stream-json`, the prompt piped on
//! stdin (Windows argument-length limits). Flags per mode and auth follow docs/PLAN.md §4.
//!
//! Every stdout line is appended to the transcript, parsed by [`ClaudeParser`] and forwarded
//! as normalised `AgentEvent`s. The terminal `{"type":"result"}` line carries the final text,
//! the structured output and the failure signals (`subtype`, `is_error`, `permission_denials`).

use super::{append_transcript, Agent, AgentOutput, AgentTask, Mode};
use crate::config::{ClaudeAuth, ProviderConfig};
use crate::events::{AgentEvent, EventSink, StageId};
use crate::proc::{self, CommandOutput, CommandSpec, StreamLine};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

const INSTRUCTION: &str = "Follow the task description provided on stdin exactly.";
const READ_ONLY_TOOLS: &str = "Read,Grep,Glob";
const EDIT_ALLOWED_TOOLS: &str = "Read,Grep,Glob,Edit,Write,Bash(cargo *),Bash(git diff *),Bash(git log *)";
/// The structured-output mechanism, not a step; hidden from the live log.
const STRUCTURED_OUTPUT_TOOL: &str = "StructuredOutput";
const FILE_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

pub(crate) const SUMMARY_CHARS: usize = 160;
pub(crate) const STDERR_TAIL_LINES: usize = 20;

pub struct ClaudeAgent {
    id: String,
    cfg: ProviderConfig,
}

impl ClaudeAgent {
    pub fn new(id: &str, cfg: &ProviderConfig) -> Self {
        Self { id: id.to_string(), cfg: cfg.clone() }
    }

    /// The full command line for `task` (program unresolved: `cfg.bin` or `claude`).
    pub(crate) fn build_command(&self, task: &AgentTask) -> CommandSpec {
        let mut spec = CommandSpec::new(self.cfg.bin.as_deref().unwrap_or("claude"), &task.cwd)
            .args(["-p", INSTRUCTION, "--output-format", "stream-json", "--verbose", "--add-dir"])
            .arg(task.artifacts.to_string_lossy());
        spec = match task.mode {
            Mode::ReadOnly => spec.args(["--tools", READ_ONLY_TOOLS]),
            Mode::Edit => spec.args([
                "--tools",
                "default",
                "--allowedTools",
                EDIT_ALLOWED_TOOLS,
                "--permission-mode",
                "acceptEdits",
            ]),
        };
        if let Some(schema) = &task.schema {
            spec = spec.arg("--json-schema").arg(schema.to_string());
        }
        spec = match self.cfg.auth {
            ClaudeAuth::ApiKey => spec.arg("--bare"),
            ClaudeAuth::Login => spec.args(["--setting-sources", "user", "--disallowedTools", "mcp__*"]),
        };
        if let Some(model) = &self.cfg.model {
            spec = spec.args(["--model", model.as_str()]);
        }
        spec.args(self.cfg.extra_args.iter().cloned()).stdin(task.prompt.clone())
    }

    fn emit(&self, events: &EventSink, stage: StageId, event: AgentEvent) {
        events.agent(stage, &self.id, event);
    }
}

#[async_trait]
impl Agent for ClaudeAgent {
    fn id(&self) -> &str {
        &self.id
    }
    fn describe(&self) -> String {
        format!("Claude Code ({})", self.cfg.model.clone().unwrap_or_else(|| "default".into()))
    }
    async fn run(&self, task: AgentTask, events: &EventSink) -> Result<AgentOutput> {
        let mut spec = self.build_command(&task);
        spec.program = proc::resolve_program(&spec.program)?.to_string_lossy().into_owned();
        append_transcript(&task.transcript, &format!("# {}", spec.display()));

        let mut parser = ClaudeParser::default();
        let mut result: Option<ResultLine> = None;
        let mut started = false;
        let mut tool_calls = 0usize;
        let mut stderr_tail: Vec<String> = Vec::new();

        let out = proc::run_streaming(&spec, task.timeout, &task.cancel, |line| match line {
            StreamLine::Stdout(l) => {
                append_transcript(&task.transcript, &l);
                for parsed in parser.parse_line(&l) {
                    match parsed {
                        Parsed::Event(ev) => {
                            match &ev {
                                AgentEvent::Started { .. } => started = true,
                                AgentEvent::ToolCall { .. } => tool_calls += 1,
                                _ => {}
                            }
                            self.emit(events, task.stage, ev);
                        }
                        Parsed::Result(r) => result = Some(r),
                    }
                }
            }
            StreamLine::Stderr(l) => {
                append_transcript(&task.transcript, &format!("stderr: {l}"));
                push_tail(&mut stderr_tail, l);
            }
        })
        .await?;

        if !started {
            self.emit(events, task.stage, AgentEvent::Started { session: None });
        }
        if let Some(r) = &result {
            if !r.denials.is_empty() {
                let text = format!("warning: permission denied: {}", r.denials.join(", "));
                self.emit(events, task.stage, AgentEvent::Text { text });
            }
        }
        let (ok, failure) = judge(&out, result.as_ref(), task.schema.is_some(), &stderr_tail, task.timeout);
        let summary = finish_summary(&out, tool_calls, result.as_ref());
        self.emit(events, task.stage, AgentEvent::Finished { ok, summary });

        let r = result.unwrap_or_default();
        Ok(AgentOutput {
            text: r.text,
            structured: r.structured,
            ok,
            failure,
            transcript: task.transcript.clone(),
            exit_code: out.exit_code,
            timed_out: out.timed_out,
            cancelled: out.cancelled,
        })
    }
}

/// What one stream-json line contributes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Parsed {
    Event(AgentEvent),
    Result(ResultLine),
}

/// The terminal `{"type":"result"}` line.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ResultLine {
    pub subtype: String,
    pub is_error: bool,
    pub text: String,
    pub structured: Option<Value>,
    /// `permission_denials` rendered as `Tool(summary)`.
    pub denials: Vec<String>,
    /// `errors` reported by the runtime (present on some error subtypes).
    pub errors: Vec<String>,
    pub num_turns: Option<u64>,
    pub cost_usd: Option<f64>,
}

impl ResultLine {
    fn success(&self) -> bool {
        self.subtype == "success" && !self.is_error
    }
}

/// Line parser. Stateful only to hide the `tool_result` that answers a hidden
/// `StructuredOutput` call (an error result for it is still surfaced).
#[derive(Debug, Default)]
pub(crate) struct ClaudeParser {
    hidden_tool_ids: HashSet<String>,
}

impl ClaudeParser {
    /// Non-JSON and unknown line types yield nothing.
    pub(crate) fn parse_line(&mut self, line: &str) -> Vec<Parsed> {
        let v: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        match v["type"].as_str().unwrap_or("") {
            "system" if v["subtype"] == "init" => {
                let session = v["session_id"].as_str().map(str::to_string);
                out.push(Parsed::Event(AgentEvent::Started { session }));
            }
            "assistant" => {
                for block in blocks(&v["message"]["content"]) {
                    self.assistant_block(block, &mut out);
                }
            }
            "user" => {
                for block in blocks(&v["message"]["content"]) {
                    self.user_block(block, &mut out);
                }
            }
            "result" => out.push(Parsed::Result(result_line(&v))),
            _ => {}
        }
        out
    }

    fn assistant_block(&mut self, block: &Value, out: &mut Vec<Parsed>) {
        match block["type"].as_str() {
            Some("text") => {
                let text = block["text"].as_str().unwrap_or("");
                if !text.is_empty() {
                    out.push(Parsed::Event(AgentEvent::Text { text: text.to_string() }));
                }
            }
            Some("thinking") => {
                let text = block["thinking"].as_str().unwrap_or("");
                if !text.is_empty() {
                    out.push(Parsed::Event(AgentEvent::Thinking { text: text.to_string() }));
                }
            }
            Some("tool_use") => {
                let name = block["name"].as_str().unwrap_or("tool");
                if name == STRUCTURED_OUTPUT_TOOL {
                    if let Some(id) = block["id"].as_str() {
                        self.hidden_tool_ids.insert(id.to_string());
                    }
                    return;
                }
                let input = &block["input"];
                out.push(Parsed::Event(AgentEvent::ToolCall { name: name.to_string(), summary: tool_summary(input) }));
                if FILE_TOOLS.contains(&name) {
                    if let Some(path) = input["file_path"].as_str().or_else(|| input["notebook_path"].as_str()) {
                        out.push(Parsed::Event(AgentEvent::FileChanged { path: path.to_string() }));
                    }
                }
            }
            _ => {}
        }
    }

    fn user_block(&mut self, block: &Value, out: &mut Vec<Parsed>) {
        if block["type"] != "tool_result" {
            return;
        }
        let is_error = block["is_error"].as_bool().unwrap_or(false);
        let hidden = block["tool_use_id"].as_str().map(|id| self.hidden_tool_ids.remove(id)).unwrap_or(false);
        if hidden && !is_error {
            return;
        }
        let summary = head(&one_line(&result_text(&block["content"])), SUMMARY_CHARS);
        out.push(Parsed::Event(AgentEvent::ToolResult { ok: !is_error, summary }));
    }
}

fn blocks(content: &Value) -> impl Iterator<Item = &Value> {
    content.as_array().into_iter().flatten()
}

/// One-line, truncated description of a tool call from its input.
fn tool_summary(input: &Value) -> String {
    let field = |k: &str| input[k].as_str().map(str::to_string);
    let raw = if let Some(cmd) = field("command") {
        cmd
    } else if let Some(path) = field("file_path").or_else(|| field("notebook_path")) {
        path
    } else if let Some(pattern) = field("pattern") {
        match field("path") {
            Some(dir) => format!("{pattern} in {dir}"),
            None => pattern,
        }
    } else if let Some(text) = field("description").or_else(|| field("prompt")).or_else(|| field("url")) {
        text
    } else if input.as_object().is_some_and(|o| !o.is_empty()) {
        input.to_string()
    } else {
        String::new()
    };
    head(&one_line(&raw), SUMMARY_CHARS)
}

/// Text of a `tool_result` content field (string or array of text blocks).
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn result_line(v: &Value) -> ResultLine {
    let strings = |key: &str| -> Vec<String> {
        v[key].as_array().into_iter().flatten().filter_map(|e| e.as_str().map(str::to_string)).collect()
    };
    ResultLine {
        subtype: v["subtype"].as_str().unwrap_or("").to_string(),
        is_error: v["is_error"].as_bool().unwrap_or(false),
        text: v["result"].as_str().unwrap_or("").to_string(),
        structured: v.get("structured_output").filter(|s| !s.is_null()).cloned(),
        denials: v["permission_denials"].as_array().into_iter().flatten().map(denial_label).collect(),
        errors: strings("errors"),
        num_turns: v["num_turns"].as_u64(),
        cost_usd: v["total_cost_usd"].as_f64(),
    }
}

fn denial_label(d: &Value) -> String {
    let name = d["tool_name"].as_str().unwrap_or("tool");
    let summary = tool_summary(&d["tool_input"]);
    if summary.is_empty() {
        name.to_string()
    } else {
        format!("{name}({summary})")
    }
}

/// `ok` and the failure message, in precedence order: hang guard, cancel, missing or failed
/// result line, exit code, missing structured output.
pub(crate) fn judge(
    out: &CommandOutput,
    result: Option<&ResultLine>,
    want_schema: bool,
    stderr_tail: &[String],
    timeout: Duration,
) -> (bool, Option<String>) {
    let denials: &[String] = result.map(|r| r.denials.as_slice()).unwrap_or(&[]);
    let with_denials = |msg: String| {
        if denials.is_empty() {
            msg
        } else {
            format!("{msg}; permission denied: {}", denials.join(", "))
        }
    };
    if out.timed_out {
        return (false, Some(format!("timed out after {}", fmt_duration(timeout))));
    }
    if out.cancelled {
        return (false, Some("cancelled".to_string()));
    }
    let Some(r) = result else {
        let msg = format!("no result line from claude (exit {})", exit_str(out));
        return (false, Some(with_stderr(msg, stderr_tail)));
    };
    if !r.success() {
        let mut msg = format!("result subtype {}", if r.subtype.is_empty() { "unknown" } else { &r.subtype });
        if !r.errors.is_empty() {
            msg.push_str(": ");
            msg.push_str(&r.errors.join("; "));
        }
        return (false, Some(with_denials(msg)));
    }
    if out.exit_code != Some(0) {
        return (false, Some(with_stderr(format!("exit {}", exit_str(out)), stderr_tail)));
    }
    if want_schema && r.structured.is_none() {
        return (false, Some(with_denials("no structured output".to_string())));
    }
    (true, None)
}

fn finish_summary(out: &CommandOutput, tool_calls: usize, result: Option<&ResultLine>) -> String {
    let mut s = if out.timed_out {
        "timed out".to_string()
    } else if out.cancelled {
        "cancelled".to_string()
    } else {
        format!("exit {}", exit_str(out))
    };
    s.push_str(&format!(", {tool_calls} tool calls"));
    if let Some(r) = result {
        if let Some(n) = r.num_turns {
            s.push_str(&format!(", {n} turns"));
        }
        if let Some(c) = r.cost_usd {
            s.push_str(&format!(", ${c:.2}"));
        }
    }
    s
}

// ---- helpers shared with the codex adapter ----

/// Keeps the last `STDERR_TAIL_LINES` stderr lines for failure messages.
pub(crate) fn push_tail(tail: &mut Vec<String>, line: String) {
    if tail.len() == STDERR_TAIL_LINES {
        tail.remove(0);
    }
    tail.push(line);
}

pub(crate) fn with_stderr(msg: String, tail: &[String]) -> String {
    if tail.is_empty() {
        msg
    } else {
        head(&format!("{msg}; stderr: {}", tail.join(" | ")), 600)
    }
}

pub(crate) fn exit_str(out: &CommandOutput) -> String {
    out.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string())
}

/// First `n` chars, with an ellipsis when cut.
pub(crate) fn head(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// Last `n` chars, with an ellipsis when cut.
pub(crate) fn tail(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        s.to_string()
    } else {
        let mut t = String::from("…");
        t.extend(s.chars().skip(count - n.saturating_sub(1)));
        t
    }
}

/// Collapses whitespace runs (including newlines) to single spaces.
pub(crate) fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s}s"),
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderKind;
    use crate::events::EventKind;
    use serde_json::json;
    use std::path::PathBuf;
    use tokio_util::sync::CancellationToken;

    const FIXTURE: &str = include_str!("../../tests/fixtures/claude-stream.jsonl");

    fn task(mode: Mode, schema: Option<Value>) -> AgentTask {
        AgentTask {
            stage: StageId::Rca,
            cwd: PathBuf::from("/wt"),
            artifacts: PathBuf::from("/art"),
            prompt: "find the bug\nline two".into(),
            mode,
            schema,
            transcript: PathBuf::from("/art/transcripts/rca.jsonl"),
            timeout: Duration::from_secs(30 * 60),
            cancel: CancellationToken::new(),
        }
    }

    fn cfg(auth: ClaudeAuth, model: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            kind: ProviderKind::Claude,
            model: model.map(String::from),
            auth,
            bin: None,
            extra_args: vec![],
        }
    }

    fn parse_all(text: &str) -> (Vec<AgentEvent>, Option<ResultLine>) {
        let mut parser = ClaudeParser::default();
        let mut events = Vec::new();
        let mut result = None;
        for line in text.lines() {
            for p in parser.parse_line(line) {
                match p {
                    Parsed::Event(e) => events.push(e),
                    Parsed::Result(r) => result = Some(r),
                }
            }
        }
        (events, result)
    }

    fn ok_out() -> CommandOutput {
        CommandOutput { exit_code: Some(0), ..Default::default() }
    }

    #[test]
    fn read_only_api_key_schema_model_flags() {
        let agent = ClaudeAgent::new("claude", &cfg(ClaudeAuth::ApiKey, Some("opus")));
        let schema = json!({"type": "object", "properties": {"root_cause": {"type": "string"}}});
        let spec = agent.build_command(&task(Mode::ReadOnly, Some(schema.clone())));
        assert_eq!(spec.program, "claude");
        assert_eq!(spec.cwd, PathBuf::from("/wt"));
        assert_eq!(spec.stdin.as_deref(), Some("find the bug\nline two"));
        let a = &spec.args;
        assert_eq!(&a[..7], &["-p", INSTRUCTION, "--output-format", "stream-json", "--verbose", "--add-dir", "/art"]);
        assert_eq!(&a[7..9], &["--tools", "Read,Grep,Glob"]);
        let schema_arg = schema.to_string();
        assert_eq!(&a[9..11], &["--json-schema", schema_arg.as_str()]);
        assert_eq!(&a[11..], &["--bare", "--model", "opus"]);
        assert!(!a.iter().any(|x| x == "--allowedTools" || x == "--setting-sources" || x == "--permission-mode"));
    }

    #[test]
    fn edit_login_bin_extra_args_flags() {
        let mut c = cfg(ClaudeAuth::Login, None);
        c.bin = Some("/opt/bin/claude.cmd".into());
        c.extra_args = vec!["--max-turns".into(), "40".into()];
        let spec = ClaudeAgent::new("claude", &c).build_command(&task(Mode::Edit, None));
        assert_eq!(spec.program, "/opt/bin/claude.cmd");
        let a = &spec.args;
        assert_eq!(
            &a[7..13],
            &["--tools", "default", "--allowedTools", EDIT_ALLOWED_TOOLS, "--permission-mode", "acceptEdits"]
        );
        assert_eq!(&a[13..], &["--setting-sources", "user", "--disallowedTools", "mcp__*", "--max-turns", "40"]);
        assert!(!a.iter().any(|x| x == "--bare" || x == "--json-schema" || x == "--model"));
    }

    #[test]
    fn fixture_parses_to_expected_events_and_result() {
        let (events, result) = parse_all(FIXTURE);
        let expected = vec![
            AgentEvent::Started { session: Some("8f1c2a3b-0000-4000-8000-000000000001".into()) },
            AgentEvent::Thinking {
                text: "The failing assertion is in parse_trims; I should read the parser before guessing.".into(),
            },
            AgentEvent::Text { text: "Let me read the parser first.".into() },
            AgentEvent::ToolCall { name: "Read".into(), summary: "src/parser.rs".into() },
            AgentEvent::ToolResult {
                ok: true,
                summary: "1 pub fn parse(input: &str) -> Result<Ast> { 2 let tokens = lex(input)?; 3 build(tokens) 4 }".into(),
            },
            AgentEvent::ToolCall { name: "Grep".into(), summary: "fn lex in src".into() },
            AgentEvent::ToolResult {
                ok: true,
                summary: "src/lexer.rs:12:pub fn lex(input: &str) -> Result<Vec<Token>> {".into(),
            },
            AgentEvent::ToolCall { name: "Edit".into(), summary: "src/parser.rs".into() },
            AgentEvent::FileChanged { path: "src/parser.rs".into() },
            AgentEvent::ToolResult { ok: true, summary: "The file src/parser.rs has been updated successfully.".into() },
            AgentEvent::ToolCall { name: "Bash".into(), summary: "cargo test -p core parse_trims".into() },
            AgentEvent::ToolResult {
                ok: false,
                summary: "error: test failed, to rerun pass `-p core --test parse_trims` thread 'parse_trims' panicked at tests/parse_trims.rs:9:5: assertion failed: ast.is_ok()".into(),
            },
            // StructuredOutput tool_use and its tool_result are hidden.
            AgentEvent::Text { text: "Root cause: lex() does not trim leading whitespace.".into() },
        ];
        assert_eq!(events, expected);

        let r = result.expect("result line");
        assert!(r.success());
        assert_eq!(r.text, "Root cause: lex() does not trim leading whitespace.");
        assert_eq!(r.structured.as_ref().and_then(|s| s["confidence"].as_str()), Some("high"));
        assert_eq!(r.denials, vec!["Bash(rm -rf target)"]);
        assert_eq!(r.num_turns, Some(6));
        assert_eq!(r.cost_usd, Some(0.1234));
    }

    #[test]
    fn structured_output_tool_is_hidden_but_its_error_is_not() {
        let mut p = ClaudeParser::default();
        let call = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t9","name":"StructuredOutput","input":{"x":1}}]}}"#;
        assert!(p.parse_line(call).is_empty());
        let bad = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t9","is_error":true,"content":"missing required field confidence"}]}}"#;
        assert_eq!(
            p.parse_line(bad),
            vec![Parsed::Event(AgentEvent::ToolResult {
                ok: false,
                summary: "missing required field confidence".into()
            })]
        );
    }

    #[test]
    fn junk_and_unknown_lines_are_ignored() {
        let mut p = ClaudeParser::default();
        assert!(p.parse_line("warning: not json at all").is_empty());
        assert!(p.parse_line("").is_empty());
        assert!(p.parse_line(r#"{"type":"stream_event","event":{}}"#).is_empty());
        assert!(p.parse_line(r#"{"type":"system","subtype":"compact_boundary"}"#).is_empty());
        assert!(p.parse_line(r#"{"type":"user","message":{"content":"plain string content"}}"#).is_empty());
    }

    #[test]
    fn tool_summaries_pick_the_right_field_and_truncate() {
        assert_eq!(tool_summary(&json!({"file_path": "a.rs", "old_string": "x"})), "a.rs");
        assert_eq!(tool_summary(&json!({"pattern": "fn main", "path": "src"})), "fn main in src");
        assert_eq!(tool_summary(&json!({"command": "ls\n-la", "description": "list"})), "ls -la");
        assert_eq!(tool_summary(&json!({"description": "sub task"})), "sub task");
        assert_eq!(tool_summary(&json!({})), "");
        let long = "x".repeat(400);
        let s = tool_summary(&json!({"command": long}));
        assert_eq!(s.chars().count(), SUMMARY_CHARS);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn judge_error_subtype_mentions_denials() {
        let r = ResultLine {
            subtype: "error_max_structured_output_retries".into(),
            denials: vec!["Bash(rm -rf target)".into()],
            ..Default::default()
        };
        let (ok, failure) = judge(
            &CommandOutput { exit_code: Some(1), ..Default::default() },
            Some(&r),
            true,
            &[],
            Duration::from_secs(60),
        );
        assert!(!ok);
        let f = failure.unwrap();
        assert!(f.starts_with("result subtype error_max_structured_output_retries"), "{f}");
        assert!(f.contains("permission denied: Bash(rm -rf target)"), "{f}");
    }

    #[test]
    fn judge_success_paths() {
        let success = ResultLine { subtype: "success".into(), text: "done".into(), ..Default::default() };
        assert_eq!(judge(&ok_out(), Some(&success), false, &[], Duration::from_secs(60)), (true, None));
        let (ok, f) = judge(&ok_out(), Some(&success), true, &[], Duration::from_secs(60));
        assert!(!ok);
        assert_eq!(f.as_deref(), Some("no structured output"));
        let with_structured = ResultLine { structured: Some(json!({"a": 1})), ..success.clone() };
        assert_eq!(judge(&ok_out(), Some(&with_structured), true, &[], Duration::from_secs(60)), (true, None));
        let denied = ResultLine { denials: vec!["Edit(x.rs)".into()], ..with_structured };
        assert_eq!(judge(&ok_out(), Some(&denied), true, &[], Duration::from_secs(60)), (true, None));
    }

    #[test]
    fn judge_process_level_failures() {
        let timed = CommandOutput { timed_out: true, ..Default::default() };
        assert_eq!(
            judge(&timed, None, false, &[], Duration::from_secs(1800)).1.as_deref(),
            Some("timed out after 30m")
        );
        let cancelled = CommandOutput { cancelled: true, ..Default::default() };
        assert_eq!(judge(&cancelled, None, false, &[], Duration::from_secs(90)).1.as_deref(), Some("cancelled"));
        let crashed = CommandOutput { exit_code: Some(1), ..Default::default() };
        let f = judge(&crashed, None, false, &["Error: not logged in".into()], Duration::from_secs(1)).1.unwrap();
        assert_eq!(f, "no result line from claude (exit 1); stderr: Error: not logged in");
        let success = ResultLine { subtype: "success".into(), ..Default::default() };
        let f = judge(&crashed, Some(&success), false, &[], Duration::from_secs(1)).1.unwrap();
        assert_eq!(f, "exit 1");
    }

    #[test]
    fn helpers() {
        assert_eq!(head("héllo", 3), "hé…");
        assert_eq!(tail("héllo", 3), "…lo");
        assert_eq!(head("abc", 3), "abc");
        assert_eq!(one_line("a \n  b\tc"), "a b c");
        assert_eq!(fmt_duration(Duration::from_secs(45)), "45s");
        assert_eq!(fmt_duration(Duration::from_secs(1800)), "30m");
        assert_eq!(fmt_duration(Duration::from_secs(90)), "1m30s");
        let mut t = Vec::new();
        for i in 0..25 {
            push_tail(&mut t, i.to_string());
        }
        assert_eq!(t.len(), STDERR_TAIL_LINES);
        assert_eq!(t[0], "5");
    }

    /// Writes a script that prints the fixture and returns its path (acts as the `claude` binary).
    fn fake_binary(dir: &std::path::Path) -> PathBuf {
        std::fs::write(dir.join("stream.jsonl"), FIXTURE).unwrap();
        #[cfg(windows)]
        {
            let p = dir.join("fake-claude.cmd");
            std::fs::write(&p, "@echo off\r\ntype \"%~dp0stream.jsonl\"\r\necho claude stderr noise 1>&2\r\n").unwrap();
            p
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.join("fake-claude.sh");
            std::fs::write(&p, "#!/bin/sh\ncat \"$(dirname \"$0\")/stream.jsonl\"\necho claude stderr noise 1>&2\n")
                .unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        }
    }

    #[tokio::test]
    async fn run_streams_fixture_through_a_fake_binary() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(ClaudeAuth::ApiKey, None);
        c.bin = Some(fake_binary(dir.path()).to_string_lossy().into_owned());
        let agent = ClaudeAgent::new("claude", &c);
        let mut t = task(Mode::ReadOnly, Some(json!({"type": "object"})));
        t.cwd = dir.path().to_path_buf();
        t.artifacts = dir.path().join("artifacts");
        t.transcript = t.artifacts.join("transcripts").join("rca.jsonl");

        let sink = EventSink::memory("run1");
        let mut rx = sink.subscribe();
        let out = agent.run(t.clone(), &sink).await.unwrap();

        assert!(out.ok, "{out:?}");
        assert_eq!(out.failure, None);
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.text, "Root cause: lex() does not trim leading whitespace.");
        assert_eq!(out.structured.as_ref().and_then(|s| s["confidence"].as_str()), Some("high"));
        assert_eq!(out.transcript, t.transcript);

        let transcript = std::fs::read_to_string(&t.transcript).unwrap();
        assert!(transcript.starts_with("# "), "command line is recorded first");
        assert!(transcript.contains("\"type\":\"result\""));
        assert!(transcript.contains("this line is not JSON"));
        assert!(transcript.contains("stderr: claude stderr noise"));

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let EventKind::Agent { stage, provider, event } = ev.kind {
                assert_eq!(stage, StageId::Rca);
                assert_eq!(provider, "claude");
                events.push(event);
            }
        }
        assert!(matches!(events.first(), Some(AgentEvent::Started { session: Some(_) })));
        assert_eq!(events.iter().filter(|e| matches!(e, AgentEvent::Started { .. })).count(), 1);
        assert_eq!(events.iter().filter(|e| matches!(e, AgentEvent::ToolCall { .. })).count(), 4);
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::Text { text } if text.contains("permission denied: Bash(rm -rf target)"))
        ));
        assert_eq!(
            events.last(),
            Some(&AgentEvent::Finished { ok: true, summary: "exit 0, 4 tool calls, 6 turns, $0.12".into() })
        );
    }

    #[tokio::test]
    async fn run_fails_to_resolve_missing_binary() {
        let mut c = cfg(ClaudeAuth::ApiKey, None);
        c.bin = Some("lyra-definitely-missing-claude-binary".into());
        let err =
            ClaudeAgent::new("claude", &c).run(task(Mode::ReadOnly, None), &EventSink::memory("r")).await.unwrap_err();
        assert!(err.to_string().contains("not found on PATH"), "{err}");
    }
}
