//! Environment checks for `lyra doctor` and `GET /api/doctor`: runtimes, `gh` (and its
//! authentication), `git` and `cargo`.

use crate::config::{Config, ProviderKind};
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

pub async fn run(config: &Config) -> Report {
    let mut r = Report::default();
    for (id, p) in &config.providers {
        let default_bin = match p.kind {
            ProviderKind::Claude => "claude",
            ProviderKind::Codex => "codex",
        };
        let bin = p.bin.clone().unwrap_or_else(|| default_bin.to_string());
        let selected = *id == config.run.provider;
        match version(&bin).await {
            Ok(v) => r.push(true, format!("provider '{id}': {bin} {v}{}", if selected { " (selected)" } else { "" })),
            // Only the selected runtime is required; others are reported but do not fail doctor.
            Err(e) if !selected => r.push(true, format!("provider '{id}' (optional, not selected): {e}")),
            Err(e) => r.push(false, format!("provider '{id}' (selected): {e}")),
        }
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
    for tool in ["git", "cargo"] {
        match version(tool).await {
            Ok(v) => r.push(true, format!("{tool}: {v}")),
            Err(e) => r.push(false, format!("{tool}: {e}")),
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
}
