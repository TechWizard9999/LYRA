//! Configuration: `~/.lyra/config.toml` (or `$LYRA_HOME/config.toml`) plus an optional
//! `<repo>/lyra.toml` that can override the verify commands.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub run: RunConfig,
    pub server: ServerConfig,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub verify: VerifyConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunConfig {
    /// Provider id: a key of `[providers]` (`claude` | `codex`).
    pub provider: String,
    /// Hang guard for one agent call or one verify command. Not a budget.
    pub timeout_minutes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
    pub open_browser: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeAuth {
    /// `--bare`, requires `ANTHROPIC_API_KEY`.
    ApiKey,
    /// Uses the `claude` login; project settings and MCP servers are excluded.
    Login,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub model: Option<String>,
    pub auth: ClaudeAuth,
    /// Binary name or path; defaults to `claude` / `codex`.
    pub bin: Option<String>,
    /// Extra arguments appended to every invocation.
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyConfig {
    pub build: String,
    pub test: String,
    pub fmt: String,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self { provider: "claude".into(), timeout_minutes: 30 }
    }
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self { bind: "127.0.0.1:7700".into(), open_browser: true }
    }
}
impl Default for ProviderConfig {
    fn default() -> Self {
        Self { kind: ProviderKind::Claude, model: None, auth: ClaudeAuth::ApiKey, bin: None, extra_args: Vec::new() }
    }
}
impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            build: "cargo build --workspace".into(),
            test: "cargo test --workspace".into(),
            fmt: "cargo fmt --all -- --check".into(),
        }
    }
}
impl Default for Config {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(
            "claude".to_string(),
            ProviderConfig { kind: ProviderKind::Claude, model: Some("opus".into()), ..Default::default() },
        );
        providers.insert(
            "codex".to_string(),
            ProviderConfig { kind: ProviderKind::Codex, model: None, ..Default::default() },
        );
        Self { run: RunConfig::default(), server: ServerConfig::default(), providers, verify: VerifyConfig::default() }
    }
}

/// Only the `[verify]` table may be overridden per repository.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RepoOverrides {
    verify: Option<PartialVerify>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialVerify {
    build: Option<String>,
    test: Option<String>,
    fmt: Option<String>,
}

/// `$LYRA_HOME` or `~/.lyra`.
pub fn lyra_home() -> PathBuf {
    if let Ok(p) = std::env::var("LYRA_HOME") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".lyra")
}

pub fn runs_dir(home: &Path) -> PathBuf {
    home.join("runs")
}

impl Config {
    pub fn parse(toml_text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(toml_text).context("parsing config.toml")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Loads `<home>/config.toml`, falling back to defaults when it does not exist.
    pub fn load(home: &Path) -> Result<Self> {
        let path = home.join("config.toml");
        if path.exists() {
            let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            Self::parse(&text)
        } else {
            Ok(Self::default())
        }
    }

    /// Writes a default config if none exists; returns its path.
    pub fn ensure_default(home: &Path) -> Result<PathBuf> {
        let path = home.join("config.toml");
        if !path.exists() {
            std::fs::create_dir_all(home)?;
            std::fs::write(&path, DEFAULT_CONFIG_TOML)?;
        }
        Ok(path)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.providers.contains_key(&self.run.provider),
            "run.provider = \"{}\" is not defined under [providers]",
            self.run.provider
        );
        anyhow::ensure!(self.run.timeout_minutes > 0, "run.timeout_minutes must be > 0");
        Ok(())
    }

    /// Applies `<repo>/lyra.toml` overrides (verify commands only).
    pub fn with_repo_overrides(mut self, repo: &Path) -> Result<Self> {
        let path = repo.join("lyra.toml");
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let o: RepoOverrides = toml::from_str(&text).context("parsing lyra.toml")?;
            if let Some(v) = o.verify {
                if let Some(b) = v.build {
                    self.verify.build = b;
                }
                if let Some(t) = v.test {
                    self.verify.test = t;
                }
                if let Some(f) = v.fmt {
                    self.verify.fmt = f;
                }
            }
        }
        Ok(self)
    }

    pub fn provider(&self, id: &str) -> Result<&ProviderConfig> {
        self.providers.get(id).with_context(|| format!("unknown provider '{id}'"))
    }

    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.run.timeout_minutes * 60)
    }
}

pub const DEFAULT_CONFIG_TOML: &str = r#"# LYRA configuration (see docs/PLAN.md §7)

[run]
provider = "claude"            # "claude" | "codex"
timeout_minutes = 30           # hang guard per agent call / verify command

[server]
bind = "127.0.0.1:7700"
open_browser = true

[providers.claude]
kind = "claude"
auth = "api_key"               # "api_key" (needs ANTHROPIC_API_KEY) | "login" (uses `claude` login)
model = "opus"

[providers.codex]
kind = "codex"
# model = "gpt-5.5-codex"

[verify]                       # <repo>/lyra.toml can override these
build = "cargo build --workspace"
test  = "cargo test --workspace"
fmt   = "cargo fmt --all -- --check"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_toml_parses_and_matches_defaults() {
        let cfg = Config::parse(DEFAULT_CONFIG_TOML).unwrap();
        assert_eq!(cfg.run.provider, "claude");
        assert_eq!(cfg.run.timeout_minutes, 30);
        assert_eq!(cfg.providers["claude"].kind, ProviderKind::Claude);
        assert_eq!(cfg.providers["claude"].auth, ClaudeAuth::ApiKey);
        assert_eq!(cfg.providers["codex"].kind, ProviderKind::Codex);
        assert_eq!(cfg.verify.test, "cargo test --workspace");
    }

    #[test]
    fn unknown_provider_is_rejected() {
        let err = Config::parse("[run]\nprovider = \"nope\"\n").unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn repo_overrides_only_touch_verify() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lyra.toml"),
            "[verify]\ntest = \"cargo test -p core\"\n[run]\nprovider = \"codex\"\n",
        )
        .unwrap();
        let cfg = Config::default().with_repo_overrides(dir.path()).unwrap();
        assert_eq!(cfg.verify.test, "cargo test -p core");
        assert_eq!(cfg.verify.build, "cargo build --workspace");
        assert_eq!(cfg.run.provider, "claude");
    }

    #[test]
    fn load_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg, Config::default());
        let path = Config::ensure_default(dir.path()).unwrap();
        assert!(path.exists());
    }
}
