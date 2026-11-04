//! Codex CLI adapter: `codex exec --json` with the prompt piped on stdin (`-`). Flags per
//! mode follow docs/PLAN.md §4; the structured answer goes through `--output-schema` and is
//! read back from the `-o <last-message file>` after exit.
//!
//! Every stdout line is appended to the transcript, parsed by [`parse_line`] and forwarded as
//! normalised `AgentEvent`s. `turn.completed` marks success; `turn.failed` is terminal,
//! while top-level `error` events may be followed by a successful runtime retry.

use super::claude::{exit_str, fmt_duration, head, one_line, push_tail, tail, with_stderr, SUMMARY_CHARS};
use super::{validate_extra_args, validate_output, Agent, AgentOutput, AgentTask, Mode, Transcript};
use crate::config::ProviderConfig;
use crate::events::{AgentEvent, EventSink, StageId};
use crate::proc::{self, CommandOutput, CommandSpec, StreamLine};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

pub struct CodexAgent {
    id: String,
    cfg: ProviderConfig,
}

impl CodexAgent {
    pub fn new(id: &str, cfg: &ProviderConfig) -> Self {
        Self { id: id.to_string(), cfg: cfg.clone() }
    }

    /// The full command line for `task` (program unresolved: `cfg.bin` or `codex`).
    /// The schema file named by `--output-schema` is written by `run`.
    pub(crate) fn build_command(&self, task: &AgentTask) -> CommandSpec {
        let mut spec = CommandSpec::new(self.cfg.bin.as_deref().unwrap_or("codex"), &task.cwd)
            .args(["exec", "--json", "-C"])
            .arg(task.cwd.to_string_lossy())
            .args(["--ephemeral", "--ignore-user-config", "--ignore-rules"]);
        // Keep the authentication directory, but do not execute user/project hooks,
        // external integrations, shell profiles, or inherited execution rules.
        for setting in [
            "approval_policy=\"never\"",
            "features.hooks=false",
            "features.apps=false",
            "features.multi_agent=false",
            "features.shell_snapshot=false",
            "features.shell_tool=false",
            "web_search=\"disabled\"",
            "allow_login_shell=false",
            "project_doc_max_bytes=0",
            "skills.include_instructions=false",
            "shell_environment_policy.inherit=\"core\"",
            "shell_environment_policy.ignore_default_excludes=false",
            "sandbox_workspace_write.network_access=false",
            "sandbox_workspace_write.exclude_tmpdir_env_var=true",
            "sandbox_workspace_write.exclude_slash_tmp=true",
        ] {
            spec = spec.args(["-c", setting]);
        }
        let path = toml::Value::String(task.cwd.to_string_lossy().into_owned()).to_string();
        spec = spec.args(["-c".into(), format!("projects.{path}.trust_level=\"untrusted\"")]);
        if let Some(model) = &self.cfg.model {
            spec = spec.args(["-m", model.as_str()]);
        }
        spec = match task.mode {
            // Codex reads/searches via shell tools. Read-only stages instead receive
            // bounded source evidence from the harness, so no model commands are needed.
            Mode::ReadOnly => spec.args(["--sandbox", "read-only"]),
            Mode::Edit => spec.args(["--sandbox", "workspace-write"]),
        };
        if task.schema.is_some() {
            spec = spec.arg("--output-schema").arg(schema_path(task).to_string_lossy());
        }
        spec = spec.arg("-o").arg(last_message_path(task).to_string_lossy());
        spec.clear_env = true;
        spec.env = proc::provider_env(&["CODEX_HOME", "CODEX_API_KEY", "OPENAI_API_KEY"]);
        spec.args(self.cfg.extra_args.iter().cloned()).arg("-").stdin(task.prompt.clone())
    }

    fn emit(&self, events: &EventSink, stage: StageId, event: AgentEvent) {
        events.agent(stage, &self.id, event);
    }
}

/// `<artifacts>/transcripts/<stage>-schema.json`
pub(crate) fn schema_path(task: &AgentTask) -> PathBuf {
    task.artifacts.join("transcripts").join(format!("{}-schema.json", task.stage))
}

/// `<artifacts>/transcripts/<stage>-last-message.txt`
pub(crate) fn last_message_path(task: &AgentTask) -> PathBuf {
    task.artifacts.join("transcripts").join(format!("{}-last-message.txt", task.stage))
}

#[async_trait]
impl Agent for CodexAgent {
    fn id(&self) -> &str {
        &self.id
    }
    fn describe(&self) -> String {
        format!("Codex CLI ({})", self.cfg.model.clone().unwrap_or_else(|| "default".into()))
    }
    async fn run(&self, task: AgentTask, events: &EventSink) -> Result<AgentOutput> {
        validate_extra_args(&self.cfg.extra_args, &[], &["--skip-git-repo-check"])?;
        let mut spec = self.build_command(&task);
        spec.program = proc::resolve_program(&spec.program)?.to_string_lossy().into_owned();

        let last_message = last_message_path(&task);
        if let Some(dir) = last_message.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        // A stale file from an earlier attempt must not pass for this run's answer.
        let _ = std::fs::remove_file(&last_message);
        if let Some(schema) = &task.schema {
            let path = schema_path(&task);
            std::fs::write(&path, serde_json::to_string_pretty(schema)?)
                .with_context(|| format!("writing {}", path.display()))?;
        }
        let mut transcript = Transcript::open(&task.transcript)?;
        transcript.append(&format!("# {}", spec.display()))?;
        let mut transcript_error = None;
        let mut truncation_reported = false;

        let mut message: Option<String> = None;
        let mut turn_completed: Option<Usage> = None;
        let mut failure: Option<String> = None;
        let mut started = false;
        let mut tool_calls = 0usize;
        let mut stderr_tail: Vec<String> = Vec::new();

        let out = proc::run_streaming(&spec, task.timeout, &task.cancel, |line| {
            let raw = match &line {
                StreamLine::Stdout(line) => line.clone(),
                StreamLine::Stderr(line) => format!("stderr: {line}"),
            };
            if transcript_error.is_none() {
                if let Err(error) = transcript.append(&raw) {
                    transcript_error = Some(error);
                }
            }
            if transcript.truncated && !truncation_reported {
                events.warn("provider transcript reached 8 MiB; remaining raw output omitted");
                truncation_reported = true;
            }
            match line {
                StreamLine::Stdout(l) => {
                    for parsed in parse_line(&l) {
                        match parsed {
                            Parsed::Event(ev) => {
                                match &ev {
                                    AgentEvent::Started { .. } => started = true,
                                    AgentEvent::ToolCall { .. } => tool_calls += 1,
                                    _ => {}
                                }
                                self.emit(events, task.stage, ev);
                            }
                            Parsed::Message(text) => message = Some(text),
                            Parsed::TurnCompleted(usage) => turn_completed = Some(usage),
                            Parsed::Failed(msg) => {
                                failure.get_or_insert(msg);
                            }
                        }
                    }
                }
                StreamLine::Stderr(l) => {
                    push_tail(&mut stderr_tail, l);
                }
            }
        })
        .await?;
        if let Some(error) = transcript_error {
            return Err(error.context("writing provider transcript"));
        }

        if !started {
            self.emit(events, task.stage, AgentEvent::Started { session: None });
        }
        // The final-message file is runtime output too; do not allocate an unbounded file.
        let file_text = if let Ok(file) = std::fs::File::open(&last_message) {
            use std::io::Read;
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= 1024 * 1024, "Codex final message exceeds 1 MiB");
            Some(String::from_utf8_lossy(&bytes).into_owned()).filter(|s| !s.trim().is_empty())
        } else {
            None
        };
        let text = message.or_else(|| file_text.clone()).unwrap_or_default();
        let structured = if task.schema.is_some() { extract_json(file_text.as_deref().unwrap_or(&text)) } else { None };
        let (mut ok, mut failure) = judge(
            &out,
            turn_completed.is_some(),
            failure.as_deref(),
            task.schema.is_some(),
            structured.is_some(),
            &stderr_tail,
            task.timeout,
        );
        if ok {
            if let Err(e) = validate_output(task.schema.as_ref(), structured.as_ref()) {
                ok = false;
                failure = Some(e.to_string());
            }
        }
        let summary = finish_summary(&out, tool_calls, turn_completed.as_ref());
        self.emit(events, task.stage, AgentEvent::Finished { ok, summary });

        Ok(AgentOutput {
            text,
            structured,
            ok,
            failure,
            transcript: task.transcript.clone(),
            exit_code: out.exit_code,
            timed_out: out.timed_out,
            cancelled: out.cancelled,
        })
    }
}

/// What one JSONL line contributes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Parsed {
    Event(AgentEvent),
    /// A completed `agent_message`; the last one is the final answer.
    Message(String),
    TurnCompleted(Usage),
    /// Only `turn.failed` is terminal; top-level errors may precede a successful retry.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Non-JSON and unknown line types yield nothing.
pub(crate) fn parse_line(line: &str) -> Vec<Parsed> {
    let v: Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    match v["type"].as_str().unwrap_or("") {
        "thread.started" => {
            let session = v["thread_id"].as_str().map(str::to_string);
            vec![Parsed::Event(AgentEvent::Started { session })]
        }
        "item.started" => item(&v["item"], false),
        "item.completed" => item(&v["item"], true),
        "turn.completed" => vec![Parsed::TurnCompleted(Usage {
            input_tokens: v["usage"]["input_tokens"].as_u64(),
            output_tokens: v["usage"]["output_tokens"].as_u64(),
        })],
        "turn.failed" => {
            let msg = v["error"]["message"].as_str().or_else(|| v["error"].as_str()).unwrap_or("turn failed");
            vec![
                Parsed::Event(AgentEvent::Error { message: msg.to_string() }),
                Parsed::Failed(format!("turn.failed: {msg}")),
            ]
        }
        "error" => {
            let msg = v["message"].as_str().unwrap_or("unknown error");
            vec![Parsed::Event(AgentEvent::Error { message: msg.to_string() })]
        }
        _ => Vec::new(),
    }
}

fn item(item: &Value, completed: bool) -> Vec<Parsed> {
    let text = |k: &str| item[k].as_str().unwrap_or("").to_string();
    let status = item["status"].as_str().unwrap_or("");
    let mut out = Vec::new();
    match item["type"].as_str().unwrap_or("") {
        "agent_message" if completed => {
            let t = text("text");
            out.push(Parsed::Event(AgentEvent::Text { text: t.clone() }));
            out.push(Parsed::Message(t));
        }
        "reasoning" if completed => out.push(Parsed::Event(AgentEvent::Thinking { text: text("text") })),
        "command_execution" => {
            if completed {
                let ok = item["exit_code"].as_i64().map(|c| c == 0).unwrap_or(status == "completed");
                let output = one_line(&text("aggregated_output"));
                let summary = if output.is_empty() { status.to_string() } else { tail(&output, SUMMARY_CHARS) };
                out.push(Parsed::Event(AgentEvent::ToolResult { ok, summary }));
            } else {
                let summary = head(&one_line(&text("command")), SUMMARY_CHARS);
                out.push(Parsed::Event(AgentEvent::ToolCall { name: "command".into(), summary }));
            }
        }
        "file_change" if completed => {
            let paths: Vec<String> = item["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["path"].as_str().map(str::to_string))
                .collect();
            if status == "failed" {
                let summary = head(&format!("file change failed: {}", paths.join(", ")), SUMMARY_CHARS);
                out.push(Parsed::Event(AgentEvent::ToolResult { ok: false, summary }));
            } else {
                out.extend(paths.into_iter().map(|path| Parsed::Event(AgentEvent::FileChanged { path })));
            }
        }
        "mcp_tool_call" => {
            if completed {
                let err = item["error"]["message"].as_str().or_else(|| item["error"].as_str()).unwrap_or("");
                let summary = head(&one_line(err), SUMMARY_CHARS);
                out.push(Parsed::Event(AgentEvent::ToolResult { ok: status != "failed", summary }));
            } else {
                let name = format!("mcp:{}/{}", text("server"), text("tool"));
                let args = &item["arguments"];
                let summary =
                    if args.is_null() { String::new() } else { head(&one_line(&args.to_string()), SUMMARY_CHARS) };
                out.push(Parsed::Event(AgentEvent::ToolCall { name, summary }));
            }
        }
        // The query is only reliably present on the completed item.
        "web_search" if completed => {
            let summary = head(&one_line(&text("query")), SUMMARY_CHARS);
            out.push(Parsed::Event(AgentEvent::ToolCall { name: "web_search".into(), summary }));
        }
        "todo_list" => {
            let items: Vec<String> = item["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|t| {
                    let mark = if t["completed"].as_bool().unwrap_or(false) { "x" } else { " " };
                    format!("[{mark}] {}", t["text"].as_str().unwrap_or(""))
                })
                .collect();
            if !items.is_empty() {
                out.push(Parsed::Event(AgentEvent::Text { text: format!("plan: {}", items.join("; ")) }));
            }
        }
        "error" => out.push(Parsed::Event(AgentEvent::Error { message: text("message") })),
        _ => {}
    }
    out
}

/// Parses the structured answer from the last message: strips a ```json fence if present,
/// then falls back to the outermost `{...}` when prose surrounds the object.
pub(crate) fn extract_json(text: &str) -> Option<Value> {
    let t = strip_fences(text.trim());
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Some(v);
    }
    let start = t.find('{')?;
    let end = t.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str(&t[start..=end]).ok()
}

fn strip_fences(t: &str) -> &str {
    let Some(rest) = t.strip_prefix("```") else { return t };
    let body = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric());
    let body = body.trim_end();
    body.strip_suffix("```").unwrap_or(body).trim()
}

/// `ok` and the failure message, in precedence order: hang guard, cancel, runtime failure,
/// exit code, missing `turn.completed`, missing structured output.
pub(crate) fn judge(
    out: &CommandOutput,
    turn_completed: bool,
    runtime_failure: Option<&str>,
    want_schema: bool,
    has_structured: bool,
    stderr_tail: &[String],
    timeout: Duration,
) -> (bool, Option<String>) {
    if out.timed_out {
        return (false, Some(format!("timed out after {}", fmt_duration(timeout))));
    }
    if out.cancelled {
        return (false, Some("cancelled".to_string()));
    }
    if out.output_truncated || out.drain_timed_out {
        return (false, Some("incomplete subprocess output (size limit or pipe drain timeout)".into()));
    }
    if let Some(f) = runtime_failure {
        return (false, Some(f.to_string()));
    }
    if out.exit_code != Some(0) {
        return (false, Some(with_stderr(format!("exit {}", exit_str(out)), stderr_tail)));
    }
    if !turn_completed {
        return (false, Some(with_stderr("no turn.completed from codex".to_string(), stderr_tail)));
    }
    if want_schema && !has_structured {
        return (false, Some("no structured output".to_string()));
    }
    (true, None)
}

fn finish_summary(out: &CommandOutput, tool_calls: usize, usage: Option<&Usage>) -> String {
    let mut s = if out.timed_out {
        "timed out".to_string()
    } else if out.cancelled {
        "cancelled".to_string()
    } else {
        format!("exit {}", exit_str(out))
    };
    s.push_str(&format!(", {tool_calls} tool calls"));
    if let Some(Usage { input_tokens: Some(i), output_tokens: Some(o) }) = usage {
        s.push_str(&format!(", {i} in / {o} out tokens"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClaudeAuth, ProviderKind};
    use crate::events::EventKind;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    const FIXTURE: &str = include_str!("../../tests/fixtures/codex-exec.jsonl");

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

    fn cfg(model: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            kind: ProviderKind::Codex,
            model: model.map(String::from),
            auth: ClaudeAuth::ApiKey,
            bin: None,
            extra_args: vec![],
        }
    }

    struct ParsedAll {
        events: Vec<AgentEvent>,
        message: Option<String>,
        usage: Option<Usage>,
        failure: Option<String>,
    }

    fn parse_all(text: &str) -> ParsedAll {
        let mut all = ParsedAll { events: vec![], message: None, usage: None, failure: None };
        for line in text.lines() {
            for p in parse_line(line) {
                match p {
                    Parsed::Event(e) => all.events.push(e),
                    Parsed::Message(m) => all.message = Some(m),
                    Parsed::TurnCompleted(u) => all.usage = Some(u),
                    Parsed::Failed(f) => {
                        all.failure.get_or_insert(f);
                    }
                }
            }
        }
        all
    }

    fn ok_out() -> CommandOutput {
        CommandOutput { exit_code: Some(0), ..Default::default() }
    }

    #[test]
    fn read_only_schema_model_extra_args_flags() {
        let mut c = cfg(Some("gpt-5.5-codex"));
        c.extra_args = vec!["--skip-git-repo-check".into()];
        let t = task(Mode::ReadOnly, Some(json!({"type": "object"})));
        let spec = CodexAgent::new("codex", &c).build_command(&t);
        assert_eq!(spec.program, "codex");
        assert_eq!(spec.cwd, PathBuf::from("/wt"));
        assert_eq!(spec.stdin.as_deref(), Some("find the bug\nline two"));
        let schema = schema_path(&t).to_string_lossy().into_owned();
        let last = last_message_path(&t).to_string_lossy().into_owned();
        assert!(schema.ends_with("rca-schema.json"));
        assert!(last.ends_with("rca-last-message.txt"));
        let a = &spec.args;
        assert_eq!(&a[..4], &["exec", "--json", "-C", "/wt"]);
        assert!(a.windows(2).any(|p| p == ["--output-schema", schema.as_str()]));
        assert!(a.windows(2).any(|p| p == ["-o", last.as_str()]));
        assert!(a.windows(2).any(|p| p == ["--sandbox", "read-only"]));
        assert!(a.contains(&"--ignore-user-config".into()));
        assert!(a.contains(&"--ignore-rules".into()));
        assert!(a.contains(&"features.hooks=false".into()));
        assert!(a.contains(&"features.shell_tool=false".into()));
        assert!(a.contains(&"skills.include_instructions=false".into()));
        assert!(a.contains(&"projects.\"/wt\".trust_level=\"untrusted\"".into()));
        assert!(!a.contains(&"--add-dir".into()));
        assert!(spec.clear_env);
    }

    #[test]
    fn edit_mode_without_schema_or_model() {
        let t = task(Mode::Edit, None);
        let mut c = cfg(None);
        c.bin = Some("/opt/codex.cmd".into());
        let spec = CodexAgent::new("codex", &c).build_command(&t);
        assert_eq!(spec.program, "/opt/codex.cmd");
        let a = &spec.args;
        assert!(a.windows(2).any(|p| p == ["--sandbox", "workspace-write"]));
        assert!(a.windows(2).any(|p| p == ["-c", "features.shell_tool=false"]));
        assert!(!a.iter().any(|x| x == "--output-schema" || x == "-m"));
        assert_eq!(a[a.len() - 3], "-o");
        assert_eq!(a.last().map(String::as_str), Some("-"));
    }

    #[test]
    fn fixture_parses_to_expected_events() {
        let all = parse_all(FIXTURE);
        let expected = vec![
            AgentEvent::Started { session: Some("0199b2a4-7c1e-7d3a-9e4f-5a6b7c8d9e0f".into()) },
            AgentEvent::Thinking {
                text: "**Inspecting the parser**\n\nI need to look at how lex() handles leading whitespace before changing anything.".into(),
            },
            AgentEvent::ToolCall { name: "command".into(), summary: "/bin/bash -lc 'rg -n \"fn lex\" src'".into() },
            AgentEvent::ToolResult { ok: true, summary: "src/lexer.rs:12:pub fn lex(input: &str) -> Result<Vec<Token>> {".into() },
            AgentEvent::ToolCall { name: "command".into(), summary: "/bin/bash -lc 'cargo test -p core parse_trims'".into() },
            AgentEvent::ToolResult {
                ok: false,
                summary: "running 1 test test parse_trims ... FAILED failures: parse_trims test result: FAILED. 0 passed; 1 failed; 0 ignored".into(),
            },
            AgentEvent::Text {
                text: "The test fails because lex() keeps leading whitespace; trimming the input before lexing fixes it.".into(),
            },
            AgentEvent::FileChanged { path: "/work/repo/src/parser.rs".into() },
            AgentEvent::FileChanged { path: "/work/repo/tests/parse_trims.rs".into() },
            AgentEvent::ToolCall { name: "mcp:docs/lookup".into(), summary: "{\"symbol\":\"str::trim\"}".into() },
            AgentEvent::ToolResult { ok: true, summary: "".into() },
            AgentEvent::Text {
                text: "```json\n{\"root_cause\":\"lex() does not trim leading whitespace\",\"confidence\":\"high\"}\n```".into(),
            },
        ];
        assert_eq!(all.events, expected);
        assert!(all.message.as_deref().unwrap().starts_with("```json"));
        assert_eq!(all.usage, Some(Usage { input_tokens: Some(18422), output_tokens: Some(912) }));
        assert_eq!(all.failure, None);
        let structured = extract_json(all.message.as_deref().unwrap()).unwrap();
        assert_eq!(structured["confidence"], "high");
    }

    #[test]
    fn turn_failed_is_terminal_but_transient_error_can_recover() {
        let failed = parse_all(
            "{\"type\":\"thread.started\",\"thread_id\":\"t1\"}\n{\"type\":\"turn.failed\",\"error\":{\"message\":\"model quota exceeded\"}}\n",
        );
        assert_eq!(failed.failure.as_deref(), Some("turn.failed: model quota exceeded"));
        assert_eq!(failed.events[1], AgentEvent::Error { message: "model quota exceeded".into() });
        let (ok, f) = judge(
            &CommandOutput { exit_code: Some(1), ..Default::default() },
            false,
            failed.failure.as_deref(),
            false,
            false,
            &[],
            Duration::from_secs(5),
        );
        assert!(!ok);
        assert_eq!(f.as_deref(), Some("turn.failed: model quota exceeded"));

        let err = parse_all("{\"type\":\"error\",\"message\":\"stream disconnected\"}\n");
        assert_eq!(err.failure, None);
        assert!(!judge(&ok_out(), false, None, false, false, &[], Duration::from_secs(1)).0);
        let recovered = parse_all(
            "{\"type\":\"error\",\"message\":\"stream disconnected\"}\n{\"type\":\"turn.completed\",\"usage\":{}}\n",
        );
        assert!(
            judge(
                &ok_out(),
                recovered.usage.is_some(),
                recovered.failure.as_deref(),
                false,
                false,
                &[],
                Duration::from_secs(1)
            )
            .0
        );
        assert_eq!(err.events, vec![AgentEvent::Error { message: "stream disconnected".into() }]);
    }

    #[test]
    fn junk_and_unknown_lines_are_ignored() {
        assert!(parse_line("codex: warning: something").is_empty());
        assert!(parse_line("").is_empty());
        assert!(parse_line("{\"type\":\"turn.started\"}").is_empty());
        assert!(parse_line("{\"type\":\"item.updated\",\"item\":{\"type\":\"todo_list\",\"items\":[]}}").is_empty());
        assert!(parse_line("{\"type\":\"item.started\",\"item\":{\"type\":\"agent_message\",\"text\":\"partial\"}}")
            .is_empty());
    }

    #[test]
    fn other_item_types() {
        let search = parse_line(
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"i\",\"type\":\"web_search\",\"query\":\"rust str trim\"}}",
        );
        assert_eq!(
            search,
            vec![Parsed::Event(AgentEvent::ToolCall { name: "web_search".into(), summary: "rust str trim".into() })]
        );
        let plan = parse_line("{\"type\":\"item.started\",\"item\":{\"type\":\"todo_list\",\"items\":[{\"text\":\"read\",\"completed\":true},{\"text\":\"fix\",\"completed\":false}]}}");
        assert_eq!(plan, vec![Parsed::Event(AgentEvent::Text { text: "plan: [x] read; [ ] fix".into() })]);
        let item_err =
            parse_line("{\"type\":\"item.completed\",\"item\":{\"type\":\"error\",\"message\":\"rate limited\"}}");
        assert_eq!(item_err, vec![Parsed::Event(AgentEvent::Error { message: "rate limited".into() })]);
        let patch_failed = parse_line("{\"type\":\"item.completed\",\"item\":{\"type\":\"file_change\",\"changes\":[{\"path\":\"a.rs\",\"kind\":\"update\"}],\"status\":\"failed\"}}");
        assert_eq!(
            patch_failed,
            vec![Parsed::Event(AgentEvent::ToolResult { ok: false, summary: "file change failed: a.rs".into() })]
        );
        let killed = parse_line("{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\",\"command\":\"sleep 999\",\"aggregated_output\":\"\",\"status\":\"failed\"}}");
        assert_eq!(killed, vec![Parsed::Event(AgentEvent::ToolResult { ok: false, summary: "failed".into() })]);
    }

    #[test]
    fn extract_json_strips_fences_and_tolerates_prose() {
        assert_eq!(extract_json("{\"a\":1}").unwrap()["a"], 1);
        assert_eq!(extract_json("```json\n{\"a\":1}\n```").unwrap()["a"], 1);
        assert_eq!(extract_json("```\n{\"a\":1}\n```\n").unwrap()["a"], 1);
        assert_eq!(extract_json("Here is the answer:\n{\"a\":{\"b\":2}}\nDone.").unwrap()["a"]["b"], 2);
        assert_eq!(extract_json(""), None);
        assert_eq!(extract_json("no json here"), None);
        assert_eq!(extract_json("{ not: valid }"), None);
    }

    #[test]
    fn judge_precedence() {
        assert_eq!(judge(&ok_out(), true, None, false, false, &[], Duration::from_secs(1)), (true, None));
        assert_eq!(judge(&ok_out(), true, None, true, true, &[], Duration::from_secs(1)), (true, None));
        assert_eq!(
            judge(&ok_out(), true, None, true, false, &[], Duration::from_secs(1)).1.as_deref(),
            Some("no structured output")
        );
        assert_eq!(
            judge(&ok_out(), false, None, false, false, &[], Duration::from_secs(1)).1.as_deref(),
            Some("no turn.completed from codex")
        );
        let crashed = CommandOutput { exit_code: Some(2), ..Default::default() };
        assert_eq!(
            judge(&crashed, false, None, false, false, &["error: unexpected argument".into()], Duration::from_secs(1))
                .1
                .as_deref(),
            Some("exit 2; stderr: error: unexpected argument")
        );
        let timed = CommandOutput { timed_out: true, ..Default::default() };
        assert_eq!(
            judge(&timed, true, None, false, false, &[], Duration::from_secs(1800)).1.as_deref(),
            Some("timed out after 30m")
        );
        let cancelled = CommandOutput { cancelled: true, ..Default::default() };
        assert_eq!(
            judge(&cancelled, true, None, false, false, &[], Duration::from_secs(1)).1.as_deref(),
            Some("cancelled")
        );
    }

    /// Writes a script that prints the fixture and returns its path (acts as the `codex` binary).
    /// With `write_last_message` it also writes `artifacts/transcripts/rca-last-message.txt`
    /// next to itself, as the real `codex -o` does.
    fn fake_binary(dir: &std::path::Path, write_last_message: bool) -> PathBuf {
        std::fs::write(dir.join("exec.jsonl"), FIXTURE).unwrap();
        #[cfg(windows)]
        {
            let p = dir.join("fake-codex.cmd");
            let mut script = String::from("@echo off\r\ntype \"%~dp0exec.jsonl\"\r\n");
            if write_last_message {
                script.push_str("echo {\"root_cause\":\"from file\",\"confidence\":\"high\"}> \"%~dp0artifacts\\transcripts\\rca-last-message.txt\"\r\n");
            }
            std::fs::write(&p, script).unwrap();
            p
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.join("fake-codex.sh");
            let mut script = String::from("#!/bin/sh\ncat \"$(dirname \"$0\")/exec.jsonl\"\n");
            if write_last_message {
                script.push_str("printf '%s\\n' '{\"root_cause\":\"from file\",\"confidence\":\"high\"}' > \"$(dirname \"$0\")/artifacts/transcripts/rca-last-message.txt\"\n");
            }
            std::fs::write(&p, script).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        }
    }

    #[tokio::test]
    async fn run_streams_fixture_and_reads_the_last_message_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(None);
        c.bin = Some(fake_binary(dir.path(), true).to_string_lossy().into_owned());
        let agent = CodexAgent::new("codex", &c);
        let mut t = task(Mode::Edit, Some(json!({"type": "object"})));
        t.cwd = dir.path().to_path_buf();
        t.artifacts = dir.path().join("artifacts");
        t.transcript = t.artifacts.join("transcripts").join("rca.jsonl");
        // A stale answer from an earlier attempt must be replaced by what the binary writes.
        let last = last_message_path(&t);
        std::fs::create_dir_all(last.parent().unwrap()).unwrap();
        std::fs::write(&last, "{\"root_cause\":\"stale\"}").unwrap();

        let sink = EventSink::memory("run1");
        let mut rx = sink.subscribe();
        let out = agent.run(t.clone(), &sink).await.unwrap();

        assert!(out.ok, "{out:?}");
        assert_eq!(out.failure, None);
        assert_eq!(out.exit_code, Some(0));
        assert!(out.text.starts_with("```json"), "stream message wins over the file: {}", out.text);
        assert_eq!(out.structured.as_ref().and_then(|s| s["root_cause"].as_str()), Some("from file"));
        let schema_file = std::fs::read_to_string(schema_path(&t)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&schema_file).unwrap(), json!({"type": "object"}));
        let transcript = std::fs::read_to_string(&t.transcript).unwrap();
        assert!(transcript.contains("\"type\":\"turn.completed\""));
        assert!(transcript.contains("this line is not JSON"));

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let EventKind::Agent { provider, event, .. } = ev.kind {
                assert_eq!(provider, "codex");
                events.push(event);
            }
        }
        assert!(matches!(events.first(), Some(AgentEvent::Started { session: Some(_) })));
        assert_eq!(events.iter().filter(|e| matches!(e, AgentEvent::FileChanged { .. })).count(), 2);
        assert_eq!(
            events.last(),
            Some(&AgentEvent::Finished { ok: true, summary: "exit 0, 3 tool calls, 18422 in / 912 out tokens".into() })
        );
    }

    #[tokio::test]
    async fn run_without_last_message_file_falls_back_to_stream_message() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(None);
        c.bin = Some(fake_binary(dir.path(), false).to_string_lossy().into_owned());
        let mut t = task(Mode::ReadOnly, Some(json!({"type": "object", "required": ["nope"]})));
        t.cwd = dir.path().to_path_buf();
        t.artifacts = dir.path().join("artifacts");
        t.transcript = t.artifacts.join("transcripts").join("rca.jsonl");
        let out = CodexAgent::new("codex", &c).run(t, &EventSink::memory("r")).await.unwrap();
        // No -o file: falls back to the last agent message, which parses (fences stripped).
        assert!(!out.ok, "schema-mismatched fallback must fail: {out:?}");
        assert!(out.failure.as_deref().unwrap().contains("schema validation"));
        assert_eq!(
            out.structured.as_ref().and_then(|s| s["root_cause"].as_str()),
            Some("lex() does not trim leading whitespace")
        );
    }

    #[tokio::test]
    async fn run_fails_to_resolve_missing_binary() {
        let mut c = cfg(None);
        c.bin = Some("lyra-definitely-missing-codex-binary".into());
        let err =
            CodexAgent::new("codex", &c).run(task(Mode::ReadOnly, None), &EventSink::memory("r")).await.unwrap_err();
        assert!(err.to_string().contains("not found on PATH"), "{err}");
    }
}
