//! maud templates: the home page (new-run form + runs list) and the run page (header,
//! stage strip, error panel, actions, live log, artifacts).

use crate::error::StageError;
use crate::events::StageId;
use crate::state::{Attempt, AttemptResult, RunState, StageStatus};
use maud::{html, Markup, DOCTYPE};
use serde::Deserialize;

/// The new-run form as posted by the browser (also used to re-fill it after an error).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewRunForm {
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub issue: String,
    #[serde(default)]
    pub issue_text: String,
    #[serde(default)]
    pub provider: String,
    /// Execution profile id, or empty for detection.
    #[serde(default)]
    pub profile: String,
    /// Checkbox: present ("on") when ticked.
    #[serde(default)]
    pub policy_acknowledged: Option<String>,
}

pub fn layout(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · LYRA" }
                link rel="stylesheet" href="/static/app.css";
            }
            body {
                header.top {
                    a.brand href="/" { "LYRA" }
                    span.tagline { "issue → failing test → fix → draft PR" }
                }
                main { (body) }
                script src="/static/app.js" defer {}
            }
        }
    }
}

pub struct HomeView<'a> {
    pub runs: &'a [RunState],
    pub providers: Vec<String>,
    pub default_provider: &'a str,
    pub form: &'a NewRunForm,
    pub error: Option<&'a str>,
    pub busy: bool,
}

pub fn home(v: &HomeView) -> Markup {
    let selected = if v.form.provider.is_empty() { v.default_provider } else { v.form.provider.as_str() };
    layout(
        "Runs",
        html! {
            section.card {
                h2 { "New run" }
                @if let Some(e) = v.error {
                    div.flash.error { (e) }
                }
                @if v.busy {
                    div.flash.info { "A run is active. A new run can start when it finishes or is cancelled." }
                }
                form.newrun method="post" action="/runs" {
                    label {
                        span { "Repository path" }
                        input type="text" name="repo" required placeholder="C:\\src\\my-repo or /home/me/src/my-repo" value=(v.form.repo);
                        small { "An already-cloned repository on this machine. LYRA works in its own worktree." }
                    }
                    label {
                        span { "Issue" }
                        input type="text" name="issue" required placeholder="https://github.com/owner/repo/issues/123 or #123" value=(v.form.issue);
                    }
                    label {
                        span { "Issue text (optional)" }
                        textarea name="issue_text" rows="4" placeholder="Paste the issue text instead of fetching it from GitHub." { (v.form.issue_text) }
                        small { "A valid issue reference is still required." }
                    }
                    div.row {
                        label {
                            span { "Runtime" }
                            select name="provider" {
                                @for p in &v.providers {
                                    option value=(p) selected[p.as_str() == selected] { (p) }
                                }
                            }
                        }
                        label {
                            span { "Execution profile" }
                            select name="profile" {
                                option value="" selected[v.form.profile.is_empty()] { "auto-detect" }
                                @for p in crate::profile::builtin() {
                                    option value=(p.id()) selected[v.form.profile == p.id()] { (p.id()) }
                                }
                            }
                            small { "Auto-detect chooses a profile from repository files." }
                        }
                        label.check {
                            input type="checkbox" name="policy_acknowledged" checked[v.form.policy_acknowledged.is_some()];
                            span { "I read the repository's contribution policy (required to push when it has one)" }
                        }
                    }
                    button.primary type="submit" disabled[v.busy] { "Run" }
                }
            }
            section.card {
                h2 { "Runs" }
                (runs_table(v.runs))
            }
        },
    )
}

fn runs_table(runs: &[RunState]) -> Markup {
    html! {
        @if runs.is_empty() {
            p.muted { "No runs yet." }
        } @else {
            div.runs-scroll role="region" aria-label="Runs table" tabindex="0" {
                table.runs {
                    thead { tr { th { "Run" } th { "Status" } th { "Stage" } th { "Issue" } th { "Runtime" } th { "Created" } th { "PR" } } }
                    tbody {
                        @for r in runs {
                            tr {
                                td { a href=(format!("/runs/{}", r.id)) { code { (r.id) } } }
                                td { (badge(r.status.as_str())) }
                                td { (r.stage.label()) }
                                td { (r.request.issue) }
                                td { (r.provider) }
                                td.muted { (short_ts(&r.created)) }
                                td { @if let Some(u) = r.pr_url.as_deref().and_then(https_pr_url) { a href=(u) target="_blank" rel="noopener" { "PR" } } }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub struct RunView<'a> {
    pub run: &'a RunState,
    pub artifacts: &'a [String],
    /// `artifacts/policy.md` contents when intake found a contribution / AI policy.
    pub policy: Option<&'a str>,
    /// Highest event seq at render time: the live log reloads only on newer terminal events.
    pub last_seq: u64,
    /// Message from a failed action (cancel/retry), shown once.
    pub flash: Option<&'a str>,
}

pub fn run_page(v: &RunView) -> Markup {
    let r = v.run;
    let terminal = r.status.is_terminal();
    layout(
        &r.id,
        html! {
            section.card.runhead {
                div.title {
                    h1 { code { (r.id) } }
                    (badge(r.status.as_str()))
                }
                dl.facts {
                    dt { "Issue" } dd { (issue_link(r)) }
                    dt { "Repository" } dd { code { (r.request.repo.display()) } }
                    dt { "Runtime" } dd { (r.provider) }
                    @if let Some(b) = &r.branch { dt { "Branch" } dd { code { (b) } } }
                    @if let Some(c) = &r.verified_commit { dt { "Verified" } dd { code { (short_sha(c)) } } }
                    @if let Some(u) = r.pr_url.as_deref().and_then(https_pr_url) { dt { "Draft PR" } dd { a href=(u) target="_blank" rel="noopener" { (u) } } }
                    dt { "Created" } dd.muted { (short_ts(&r.created)) }
                }
            }
            @if let Some(f) = v.flash {
                div.flash.error { (f) }
            }
            section.stages {
                @for s in StageId::ALL {
                    @let (class, label) = stage_status(r, s);
                    div class=(format!("stage {class}")) id=(format!("stage-{}", s.as_str())) {
                        div.name { (s.label()) }
                        div.status { (label) }
                        div.dur { (stage_duration(r, s).unwrap_or_default()) }
                    }
                }
            }
            @if r.last_error.is_some() || r.stop_reason.is_some() {
                section.card.errors {
                    @if let Some(reason) = &r.stop_reason {
                        h3 { "Stopped at " (r.stage.label()) ": " (reason) }
                        @if let Some(m) = stop_message(r) { pre.msg { (m) } }
                    }
                    @if let Some(e) = &r.last_error {
                        h3 { "Failed at " (r.stage.label()) " (attempt " (r.attempt_count(r.stage)) "): " (error_kind(e)) }
                        pre.msg { (e.message) }
                        p.muted {
                            @if e.retryable { "Retrying from this stage may help." } @else { "Not retryable as is: fix the cause first." }
                            @if let Some(d) = details_link(&r.id, e) { " " a href=(d) { "details" } }
                        }
                    }
                }
            }
            section.card.actions {
                @if !terminal {
                    form method="post" action=(format!("/runs/{}/cancel", r.id)) {
                        button.danger type="submit" { "Cancel" }
                    }
                }
                @if terminal {
                    form.retry-controls method="post" action=(format!("/runs/{}/retry/{}", r.id, r.stage.as_str())) {
                        @if r.has_policy && !r.request.policy_acknowledged {
                            label.check {
                                input type="checkbox" name="policy_acknowledged";
                                span { "I read the repository's contribution policy" }
                            }
                        }
                        @for s in retry_stages(r) {
                            button type="submit" formaction=(format!("/runs/{}/retry/{}", r.id, s.as_str())) { "Retry from " (s.label()) }
                        }
                    }
                }
                @if let Some(u) = r.pr_url.as_deref().and_then(https_pr_url) {
                    a.button.primary href=(u) target="_blank" rel="noopener" { "Open PR" }
                }
            }
            @if let Some(p) = v.policy {
                section.card {
                    details {
                        summary { "Repository contribution / AI policy" @if r.request.policy_acknowledged { " (acknowledged)" } @else { " (not acknowledged: the PR stage will stop)" } }
                        pre.policy { (p) }
                    }
                }
            }
            section.card {
                h2 { "Live log" }
                div #log data-run=(r.id) data-seq=(v.last_seq) data-terminal=(terminal) {}
            }
            section.card {
                h2 { "Artifacts" }
                p #no-artifacts .muted hidden[!v.artifacts.is_empty()] { "None yet." }
                ul #artifacts .artifacts {
                    @for a in v.artifacts {
                        li data-artifact=(a) {
                            a href=(format!("/runs/{}/artifacts/{}", r.id, url_path(a))) target="_blank" rel="noopener" { (a) }
                        }
                    }
                }
            }
        },
    )
}

fn https_pr_url(value: &str) -> Option<&str> {
    let uri: axum::http::Uri = value.parse().ok()?;
    (uri.scheme_str() == Some("https") && uri.authority().is_some()).then_some(value)
}

fn badge(status: &str) -> Markup {
    html! { span class=(format!("badge {status}")) { (status) } }
}

fn issue_link(r: &RunState) -> Markup {
    let issue = &r.request.issue;
    let url = if issue.starts_with("http://") || issue.starts_with("https://") {
        Some(issue.clone())
    } else {
        match (r.issue_slug.as_ref().or(r.repo_slug.as_ref()), r.issue_number) {
            (Some(slug), Some(n)) => Some(format!("https://github.com/{slug}/issues/{n}")),
            _ => None,
        }
    };
    html! {
        @match url {
            Some(u) => a href=(u) target="_blank" rel="noopener" { (issue) },
            None => (issue),
        }
        @if r.request.issue_text.is_some() { span.muted { " (pasted text)" } }
    }
}

/// CSS class and label of one stage. A stage still marked running in a finished run
/// (interrupted / cancelled) is shown as stopped with the run status.
fn stage_status(r: &RunState, s: StageId) -> (&'static str, &'static str) {
    match r.stage_status(s) {
        StageStatus::Running if r.status.is_terminal() => ("stopped", r.status.as_str()),
        st => {
            let name = stage_status_str(st);
            (name, name)
        }
    }
}

fn stage_status_str(s: StageStatus) -> &'static str {
    match s {
        StageStatus::Pending => "pending",
        StageStatus::Running => "running",
        StageStatus::Done => "done",
        StageStatus::Failed => "failed",
        StageStatus::Stopped => "stopped",
    }
}

fn current_attempt(r: &RunState, s: StageId) -> Option<&Attempt> {
    r.attempts.iter().rev().find(|a| a.stage == s && !matches!(a.result, AttemptResult::Superseded))
}

fn stage_duration(r: &RunState, s: StageId) -> Option<String> {
    if r.stage_status(s) == StageStatus::Pending {
        return None;
    }
    let a = current_attempt(r, s)?;
    let ms = match &a.result {
        AttemptResult::Completed { duration_ms } => Some(*duration_ms),
        AttemptResult::Running => None,
        _ => {
            let (start, end) = (parse_ts(&a.started)?, parse_ts(a.finished.as_deref()?)?);
            Some(((end - start).max(0) as u64) * 1000)
        }
    };
    let text = match ms {
        Some(ms) => fmt_ms(ms),
        None => format!("since {}", a.started.get(11..19).unwrap_or("")),
    };
    Some(if a.n > 1 { format!("{text} · attempt {}", a.n) } else { text })
}

fn stop_message(r: &RunState) -> Option<&str> {
    r.attempts.iter().rev().find_map(|a| match &a.result {
        AttemptResult::Stopped { message, .. } => Some(message.as_str()),
        _ => None,
    })
}

fn error_kind(e: &StageError) -> String {
    serde_json::to_value(e.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{:?}", e.kind))
}

/// `details` is relative to the run's `artifacts/` (a leading `artifacts/` is tolerated).
fn details_link(id: &str, e: &StageError) -> Option<String> {
    let d = e.details.as_ref()?.to_string_lossy().replace('\\', "/");
    let rel = d.strip_prefix("artifacts/").unwrap_or(&d).trim_start_matches('/');
    super::api::safe_artifact_path(rel)?;
    Some(format!("/runs/{id}/artifacts/{}", url_path(rel)))
}

/// Retry targets: every stage that has started, plus the current one.
fn retry_stages(r: &RunState) -> Vec<StageId> {
    StageId::ALL.into_iter().filter(|s| *s == r.stage || r.stage_status(*s) != StageStatus::Pending).collect()
}

fn short_ts(ts: &str) -> String {
    ts.get(..19).map(|s| s.replace('T', " ")).unwrap_or_else(|| ts.to_string())
}

fn short_sha(c: &str) -> &str {
    c.get(..12).unwrap_or(c)
}

fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// Unix seconds of a UTC RFC 3339 timestamp as written by `now_rfc3339` (`...Z`).
fn parse_ts(ts: &str) -> Option<i64> {
    let num = |r: std::ops::Range<usize>| ts.get(r)?.parse::<u32>().ok();
    let month = time::Month::try_from(num(5..7)? as u8).ok()?;
    let date = time::Date::from_calendar_date(num(0..4)? as i32, month, num(8..10)? as u8).ok()?;
    let t = time::Time::from_hms(num(11..13)? as u8, num(14..16)? as u8, num(17..19)? as u8).ok()?;
    Some(time::PrimitiveDateTime::new(date, t).assume_utc().unix_timestamp())
}

/// Percent-encodes a relative path for a URL, keeping `/` separators.
pub fn url_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for b in p.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ErrorKind, StopReason};
    use crate::state::RunRequest;
    use std::path::PathBuf;

    fn run() -> RunState {
        let req = RunRequest {
            repo: PathBuf::from("/src/demo"),
            issue: "https://github.com/o/demo/issues/7".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
            profile: None,
        };
        RunState::new("demo-7-20261104-101500".into(), req, "claude".into())
    }

    #[test]
    fn run_page_shows_id_stages_and_pr() {
        let mut r = run();
        r.status = crate::state::Status::Done;
        r.pr_url = Some("https://github.com/o/demo/pull/9".into());
        r.set_stage_status(StageId::Intake, StageStatus::Done);
        r.attempts.push(Attempt {
            stage: StageId::Intake,
            n: 1,
            input_commit: None,
            started: "2026-11-04T10:15:00Z".into(),
            finished: Some("2026-11-04T10:15:02Z".into()),
            result: AttemptResult::Completed { duration_ms: 2300 },
        });
        let arts = vec!["00-issue.md".to_string(), "20-repro/output.txt".to_string()];
        let html =
            run_page(&RunView { run: &r, artifacts: &arts, policy: None, last_seq: 5, flash: None }).into_string();
        assert!(html.contains("demo-7-20261104-101500"));
        for s in StageId::ALL {
            assert!(html.contains(s.label()), "missing {}", s.label());
        }
        assert!(html.contains("https://github.com/o/demo/pull/9"));
        assert!(html.contains("Open PR"));
        assert!(html.contains("2.3s"));
        assert!(html.contains("/runs/demo-7-20261104-101500/artifacts/20-repro/output.txt"));
        assert!(html.contains(r#"data-artifact="20-repro/output.txt""#));
        assert!(html.contains("Retry from S0 Intake"));
        assert!(!html.contains(">Cancel<"));
        assert!(html.contains(r#"data-seq="5""#));
    }

    #[test]
    fn run_page_shows_errors_and_cancel() {
        let mut r = run();
        r.status = crate::state::Status::Running;
        r.stage = StageId::Repro;
        r.set_stage_status(StageId::Repro, StageStatus::Running);
        let html = run_page(&RunView { run: &r, artifacts: &[], policy: Some("Be nice."), last_seq: 0, flash: None })
            .into_string();
        assert!(html.contains("Cancel"));
        assert!(!html.contains("Retry from"));
        assert!(html.contains("Be nice."));
        assert!(html.contains(r#"id="no-artifacts""#));
        assert!(html.contains(r#"id="artifacts""#));

        r.status = crate::state::Status::Failed;
        r.set_stage_status(StageId::Repro, StageStatus::Failed);
        r.last_error =
            Some(StageError::new(ErrorKind::Guard, "test <file> changed").with_details("20-repro/output.txt"));
        r.stop_reason = Some(StopReason::NotReproduced);
        let html = run_page(&RunView { run: &r, artifacts: &[], policy: None, last_seq: 0, flash: Some("oops") })
            .into_string();
        assert!(html.contains("guard"));
        assert!(html.contains("test &lt;file&gt; changed"), "message must be escaped");
        assert!(html.contains("/runs/demo-7-20261104-101500/artifacts/20-repro/output.txt"));
        assert!(html.contains("could not be reproduced"));
        assert!(html.contains("Retry from S2 Repro"));
        assert!(html.contains("oops"));
    }

    #[test]
    fn home_lists_runs_and_providers() {
        let runs = vec![run()];
        let form = NewRunForm::default();
        let html = home(&HomeView {
            runs: &runs,
            providers: vec!["claude".into(), "codex".into()],
            default_provider: "codex",
            form: &form,
            error: Some("repo path is required"),
            busy: false,
        })
        .into_string();
        assert!(html.contains(r#"<option value="codex" selected>"#));
        assert!(html.contains("repo path is required"));
        assert!(html.contains(r#"class="runs-scroll" role="region" aria-label="Runs table" tabindex="0""#));
        assert!(html.contains(r#"<a href="/runs/demo-7-20261104-101500""#));
    }

    #[test]
    fn unacknowledged_policy_has_one_shared_checkbox_for_all_retry_buttons() {
        let mut r = run();
        r.status = crate::state::Status::Stopped;
        r.stage = StageId::Pr;
        r.has_policy = true;
        for stage in StageId::ALL {
            r.set_stage_status(stage, if stage == StageId::Pr { StageStatus::Stopped } else { StageStatus::Done });
        }
        let render = |run: &RunState| {
            run_page(&RunView { run, artifacts: &[], policy: Some("Read this policy."), last_seq: 0, flash: None })
                .into_string()
        };
        let html = render(&r);
        assert_eq!(html.matches(r#"name="policy_acknowledged""#).count(), 1);
        assert!(html.contains(r#"formaction="/runs/demo-7-20261104-101500/retry/pr""#));
        assert!(html.contains(r#"formaction="/runs/demo-7-20261104-101500/retry/intake""#));
        r.request.policy_acknowledged = true;
        assert!(!render(&r).contains(r#"name="policy_acknowledged""#));
        r.request.policy_acknowledged = false;
        r.has_policy = false;
        assert!(!render(&r).contains(r#"name="policy_acknowledged""#));
    }

    #[test]
    fn helpers() {
        assert_eq!(fmt_ms(61_000), "1m 01s");
        assert_eq!(parse_ts("2026-11-04T10:15:02.5Z").unwrap() - parse_ts("2026-11-03T10:15:00Z").unwrap(), 86_402);
        assert_eq!(url_path("a b/c#d.txt"), "a%20b/c%23d.txt");
    }

    #[test]
    fn pr_links_require_https_and_scripts_are_loaded_once_on_every_page() {
        for bad in
            ["javascript:alert(1)", "data:text/html,bad", "http://github.com/o/r/pull/1", "//evil.example/x", "https:"]
        {
            let mut r = run();
            r.pr_url = Some(bad.into());
            let html =
                run_page(&RunView { run: &r, artifacts: &[], policy: None, last_seq: 0, flash: None }).into_string();
            assert!(!html.contains("Open PR"), "{bad}");
            assert!(!html.contains(&format!("href=\"{bad}\"")), "{bad}");
            assert_eq!(html.matches("src=\"/static/app.js\"").count(), 1);
            let html = runs_table(&[r]).into_string();
            assert!(!html.contains("target=\"_blank\""), "{bad}");
        }
        assert!(https_pr_url("https://github.com/o/r/pull/1").is_some());
        let form = NewRunForm::default();
        let html = home(&HomeView {
            runs: &[],
            providers: vec![],
            default_provider: "claude",
            form: &form,
            error: None,
            busy: false,
        })
        .into_string();
        assert_eq!(html.matches("src=\"/static/app.js\"").count(), 1);
    }

    #[test]
    fn shorthand_issue_links_use_the_issue_repository_when_working_in_a_fork() {
        let mut r = run();
        r.request.issue = "#7".into();
        r.issue_number = Some(7);
        r.repo_slug = Some("contributor/demo".into());
        r.issue_slug = Some("upstream/demo".into());
        let html = issue_link(&r).into_string();
        assert!(html.contains("https://github.com/upstream/demo/issues/7"));
        assert!(!html.contains("https://github.com/contributor/demo/issues/7"));
        r.issue_slug = None;
        assert!(issue_link(&r).into_string().contains("https://github.com/contributor/demo/issues/7"));
    }
}
