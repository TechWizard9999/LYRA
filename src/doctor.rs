//! Environment checks for `lyra doctor` and `POST /api/doctor`: runtimes, local provider
//! authentication, `gh` (and its authentication), `git` and `cargo`. No model calls are made.

use crate::config::{ClaudeAuth, Config, ProviderConfig, ProviderKind};
use crate::proc::{self, CommandSpec};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Default)]
pub struct Report {
    pub lines: Vec<(bool, String)>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.lines.iter().all(|(ok, _)| *ok)
    }
    pub fn render_text(&self) -> String {
        self.lines
            .iter()
            .map(|(ok, l)| format!("{} {l}", if *ok { "ok " } else { "!! " }))
            .collect::<Vec<_>>()
            .join("\n")
    }
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "ok": self.ok(),
            "checks": self.lines.iter().map(|(ok, text)| serde_json::json!({ "ok": ok, "text": text })).collect::<Vec<_>>(),
        })
    }
    fn push(&mut self, ok: bool, text: impl Into<String>) {
        self.lines.push((ok, text.into()));
    }
}

/// `<program> --version` → first output line, or the failure.
async fn version(program: &str) -> Result<String, String> {
    let path = proc::resolve_program(program).map_err(|e| format!("{e:#}"))?;
    let spec = CommandSpec::new(path.to_string_lossy().to_string(), std::env::temp_dir()).arg("--version");
    match proc::run(&spec, Duration::from_secs(60), &CancellationToken::new()).await {
        Ok(out) if out.success() => {
            let text = if out.stdout.trim().is_empty() { &out.stderr } else { &out.stdout };
            Ok(text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("(no version output)").to_string())
        }
        Ok(out) if out.timed_out => Err(format!("{} --version timed out", path.display())),
        Ok(out) => {
            Err(format!("{} --version exited with {:?}: {}", path.display(), out.exit_code, out.combined().trim()))
        }
        Err(e) => Err(format!("{e:#}")),
    }
}

/// Explicit environment credentials take precedence over a saved login, just as in the
/// adapters. Presence is a local preflight check, never a claim that a key is valid.
fn environment_auth(cfg: &ProviderConfig, env: &[(String, String)]) -> Result<Option<&'static str>, String> {
    let keys: &[&str] = match (cfg.kind, cfg.auth) {
        (ProviderKind::Claude, ClaudeAuth::ApiKey) => &["ANTHROPIC_API_KEY"],
        (ProviderKind::Claude, ClaudeAuth::Login) => &["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"],
        (ProviderKind::Codex, _) => &["CODEX_API_KEY", "OPENAI_API_KEY"],
    };
    for key in keys {
        if env.iter().any(|(name, value)| name.eq_ignore_ascii_case(key) && !value.trim().is_empty()) {
            return Ok(Some(key));
        }
    }
    if cfg.kind == ProviderKind::Claude && cfg.auth == ClaudeAuth::ApiKey {
        Err("auth=api_key requires nonempty ANTHROPIC_API_KEY (or choose auth=\"login\")".into())
    } else {
        Ok(None)
    }
}

async fn authentication(program: &str, cfg: &ProviderConfig, env: &[(String, String)]) -> Result<String, String> {
    if let Some(key) = environment_auth(cfg, env)? {
        return Ok(format!("{key} is configured (not remotely validated)"));
    }
    let path = proc::resolve_program(program).map_err(|e| format!("{e:#}"))?;
    let (args, login): (&[&str], &str) = match cfg.kind {
        ProviderKind::Claude => {
            (&["--restricted", "--safe-mode", "--setting-sources", "", "auth", "status", "--json"], "claude auth login")
        }
        ProviderKind::Codex => (&["login", "status", "-c", "features.hooks=false"], "codex login"),
    };
    let mut spec = CommandSpec::new(path.to_string_lossy(), std::env::temp_dir()).args(args.iter().copied());
    spec.clear_env = true;
    spec.env = env.to_vec();
    let out = proc::run(&spec, Duration::from_secs(60), &CancellationToken::new())
        .await
        .map_err(|e| format!("authentication status check failed: {e:#}"))?;
    if out.timed_out {
        return Err("authentication status check timed out".into());
    }
    if out.output_truncated || out.drain_timed_out {
        return Err("authentication status check returned incomplete output".into());
    }
    if !out.success() {
        // Status output may contain account details or a masked key. Keep it out of
        // diagnostics, including the dashboard's JSON response.
        return Err(format!(
            "not logged in or authentication status unavailable (exit {:?}; run `{login}`)",
            out.exit_code
        ));
    }
    if cfg.kind == ProviderKind::Claude {
        let status: serde_json::Value = serde_json::from_str(&out.stdout)
            .map_err(|_| "claude auth status did not return valid JSON".to_string())?;
        if status["loggedIn"].as_bool() != Some(true) {
            return Err(format!("not logged in (run `{login}`)"));
        }
    }
    Ok("login credentials present (not remotely validated)".into())
}

async fn provider_report(id: &str, cfg: &ProviderConfig, selected: bool, env: &[(String, String)]) -> Report {
    let mut report = Report::default();
    let default_bin = match cfg.kind {
        ProviderKind::Claude => "claude",
        ProviderKind::Codex => "codex",
    };
    let bin = cfg.bin.as_deref().unwrap_or(default_bin);
    let label = if selected { " (selected)" } else { " (optional, not selected)" };
    match version(bin).await {
        Ok(v) => {
            report.push(true, format!("provider '{id}'{label}: {bin} {v}"));
            match authentication(bin, cfg, env).await {
                Ok(auth) => report.push(true, format!("provider '{id}' authentication{label}: {auth}")),
                Err(error) => report.push(!selected, format!("provider '{id}' authentication{label}: {error}")),
            }
        }
        // Only the selected runtime is required; others do not fail doctor.
        Err(error) => report.push(!selected, format!("provider '{id}'{label}: {error}")),
    }
    report
}

pub async fn run(config: &Config) -> Report {
    let mut r = Report::default();
    for (id, p) in &config.providers {
        let keys: &[&str] = match p.kind {
            ProviderKind::Claude => &["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CONFIG_DIR"],
            ProviderKind::Codex => &["CODEX_HOME", "CODEX_API_KEY", "OPENAI_API_KEY"],
        };
        r.lines.extend(provider_report(id, p, *id == config.run.provider, &proc::provider_env(keys)).await.lines);
    }
    if !config.providers.contains_key(&config.run.provider) {
        r.push(false, format!("run.provider '{}' is not configured under [providers]", config.run.provider));
    }
    match version("gh").await {
        Ok(v) => {
            r.push(true, format!("gh: {v}"));
            let authed = tokio::task::spawn_blocking(crate::github::auth_ok).await.unwrap_or(false);
            r.push(
                authed,
                if authed {
                    "gh auth status: logged in".to_string()
                } else {
                    "gh auth status: not logged in (run `gh auth login`)".to_string()
                },
            );
        }
        Err(e) => r.push(false, format!("gh: {e}")),
    }
    match version("git").await {
        Ok(v) => r.push(true, format!("git: {v}")),
        Err(e) => r.push(false, format!("git: {e}")),
    }
    // Each built-in execution profile names the tools it runs on the target repository. A
    // machine only needs the tools of the repositories it works on, so a missing tool is
    // reported but does not fail the check; Intake refuses a run whose profile lacks them.
    for profile in crate::profile::builtin() {
        for tool in profile.required_tools() {
            match version(tool).await {
                Ok(v) => r.push(true, format!("{tool} ({} profile): {v}", profile.id())),
                Err(e) => r.push(
                    true,
                    format!(
                        "{tool} ({} profile): not available, needed only for {} repositories ({e})",
                        profile.id(),
                        profile.id()
                    ),
                ),
            }
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_and_json() {
        let mut r = Report::default();
        r.push(true, "git: git version 2.50");
        r.push(false, "gh: 'gh' not found on PATH");
        assert!(!r.ok());
        assert_eq!(r.render_text(), "ok  git: git version 2.50\n!!  gh: 'gh' not found on PATH");
        let j = r.to_json();
        assert_eq!(j["ok"], false);
        assert_eq!(j["checks"][1]["ok"], false);
        assert_eq!(j["checks"][0]["text"], "git: git version 2.50");
        assert!(Report::default().ok());
    }

    #[tokio::test]
    async fn missing_program_is_reported() {
        let e = version("lyra-definitely-not-a-program").await.unwrap_err();
        assert!(e.contains("not found"), "{e}");
    }

    fn fake_runtime(dir: &std::path::Path, output: &str, exit_code: i32) -> String {
        std::fs::write(dir.join("auth-output.txt"), output).unwrap();
        #[cfg(windows)]
        let path = {
            let path = dir.join("fake-runtime.cmd");
            std::fs::write(
                &path,
                format!(
                    "@echo off\r\nif \"%~1\"==\"--version\" (\r\n  echo fixture-runtime 1.0\r\n  exit /b 0\r\n)\r\necho %*> \"%~dp0auth-args.txt\"\r\ntype \"%~dp0auth-output.txt\"\r\nexit /b {exit_code}\r\n"
                ),
            )
            .unwrap();
            path
        };
        #[cfg(not(windows))]
        let path = {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join("fake-runtime.sh");
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\nif [ \"$1\" = --version ]; then\n  echo 'fixture-runtime 1.0'\n  exit 0\nfi\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/auth-args.txt\"\ncat \"$(dirname \"$0\")/auth-output.txt\"\nexit {exit_code}\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        path.to_string_lossy().into_owned()
    }

    fn login_config(kind: ProviderKind, bin: String) -> ProviderConfig {
        ProviderConfig { kind, auth: ClaudeAuth::Login, bin: Some(bin), ..Default::default() }
    }

    #[tokio::test]
    async fn successful_version_does_not_hide_missing_login() {
        for (kind, output, exit_code) in [
            (ProviderKind::Claude, r#"{"loggedIn":false}"#, 1),
            (ProviderKind::Claude, r#"{"loggedIn":false}"#, 0),
            (ProviderKind::Codex, "Not logged in", 1),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let cfg = login_config(kind, fake_runtime(dir.path(), output, exit_code));
            let report = provider_report("selected", &cfg, true, &proc::scrubbed_env()).await;
            assert!(report.lines[0].0, "the executable's version check should pass");
            assert!(!report.ok(), "missing login must fail doctor: {}", report.render_text());
            assert!(report.render_text().contains("authentication (selected): not logged in"));
        }
    }

    #[tokio::test]
    async fn saved_login_is_healthy_without_a_model_call() {
        for (kind, output) in [
            (ProviderKind::Claude, r#"{"loggedIn":true,"email":"private@example.com"}"#),
            (ProviderKind::Codex, "Logged in using ChatGPT: private@example.com"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let cfg = login_config(kind, fake_runtime(dir.path(), output, 0));
            let report = provider_report("selected", &cfg, true, &proc::scrubbed_env()).await;
            assert!(report.ok(), "{}", report.render_text());
            assert!(report.render_text().contains("login credentials present (not remotely validated)"));
            assert!(!report.render_text().contains("private@example.com"));
            let args = std::fs::read_to_string(dir.path().join("auth-args.txt")).unwrap();
            assert!(args.contains("status"));
            assert!(!args.contains("exec"));
            assert!(!args.contains("--print"));
        }
    }

    #[tokio::test]
    async fn claude_api_key_mode_cannot_use_a_saved_login_or_oauth_token() {
        let dir = tempfile::tempdir().unwrap();
        let cfg =
            ProviderConfig { bin: Some(fake_runtime(dir.path(), r#"{"loggedIn":true}"#, 0)), ..Default::default() };
        for extra in [
            vec![],
            vec![("ANTHROPIC_API_KEY".into(), "  ".into())],
            vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "fixture-token".into())],
        ] {
            let mut env = proc::scrubbed_env();
            env.extend(extra);
            let report = provider_report("selected", &cfg, true, &env).await;
            assert!(!report.ok());
            assert!(report.render_text().contains("requires nonempty ANTHROPIC_API_KEY"));
            assert!(!dir.path().join("auth-args.txt").exists(), "API-key mode must not inspect a saved login");
        }
    }

    #[tokio::test]
    async fn environment_credentials_are_reported_as_configured_without_exposing_them() {
        for (kind, auth, key) in [
            (ProviderKind::Claude, ClaudeAuth::ApiKey, "ANTHROPIC_API_KEY"),
            (ProviderKind::Claude, ClaudeAuth::Login, "CLAUDE_CODE_OAUTH_TOKEN"),
            (ProviderKind::Codex, ClaudeAuth::ApiKey, "CODEX_API_KEY"),
            (ProviderKind::Codex, ClaudeAuth::Login, "OPENAI_API_KEY"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let cfg = ProviderConfig {
                kind,
                auth,
                bin: Some(fake_runtime(dir.path(), "this login status must not be consulted", 1)),
                ..Default::default()
            };
            let mut env = proc::scrubbed_env();
            env.push((key.into(), "private-fixture-credential".into()));
            let report = provider_report("selected", &cfg, true, &env).await;
            assert!(report.ok(), "{}", report.render_text());
            assert!(report.render_text().contains(&format!("{key} is configured (not remotely validated)")));
            assert!(!report.render_text().contains("private-fixture-credential"));
            assert!(!dir.path().join("auth-args.txt").exists());
        }
    }

    #[tokio::test]
    async fn missing_auth_for_an_optional_provider_does_not_fail_doctor() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = login_config(ProviderKind::Codex, fake_runtime(dir.path(), "Not logged in", 1));
        let report = provider_report("optional", &cfg, false, &proc::scrubbed_env()).await;
        assert!(report.ok());
        assert!(report.render_text().contains("authentication (optional, not selected): not logged in"));
    }
}
