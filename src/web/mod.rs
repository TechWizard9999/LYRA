//! The local dashboard (`lyra serve`): an axum server with maud-rendered pages, a JSON API
//! and an SSE stream of run events. Single binary, no JS build; CSS and the small live-log
//! script are compiled in from `static/`.
//!
//! - `server` : `AppState`, the router, HTML page handlers and static assets
//! - `api`    : JSON API handlers and the shared error type
//! - `sse`    : replay-then-live event stream for one run
//! - `views`  : maud templates (home page, run page)

mod api;
mod server;
mod sse;
mod views;

pub use server::{router, AppState};

use anyhow::Context;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::engine::Engine;

/// Binds `bind`, prints the dashboard URL, optionally opens the browser and serves until
/// the process exits. Runs left `running` by a previous process are marked interrupted first.
pub async fn serve(engine: Arc<Engine>, bind: &str, open_browser: bool) -> anyhow::Result<()> {
    let address = loopback_bind(bind)?;
    let listener = tokio::net::TcpListener::bind(address).await.with_context(|| format!("binding {bind}"))?;
    let interrupted = engine.mark_interrupted()?;
    if interrupted > 0 {
        println!("marked {interrupted} unfinished run(s) as interrupted (Retry is available on the run page)");
    }
    let address = listener.local_addr()?;
    let app = router(AppState::new(engine.clone(), address));
    let url = display_url(address);
    println!("LYRA dashboard: {url}");
    if open_browser {
        if let Err(e) = open::that_detached(&url) {
            eprintln!("could not open a browser ({e}); open {url} yourself");
        }
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            engine.cancel_all();
            while engine.is_busy() {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .context("serving the dashboard")?;
    Ok(())
}

fn loopback_bind(bind: &str) -> anyhow::Result<SocketAddr> {
    // Do not resolve arbitrary hostnames: accept literal loopback addresses and
    // the conventional localhost alias only, before opening a listening socket.
    let normalized = bind.strip_prefix("localhost:").map(|port| format!("127.0.0.1:{port}"));
    let address: SocketAddr = normalized.as_deref().unwrap_or(bind).parse().context("expected a loopback IP:port")?;
    anyhow::ensure!(address.ip().is_loopback(), "the dashboard only supports loopback binds (127.0.0.1 or ::1)");
    Ok(address)
}

fn display_url(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_binds_are_accepted() {
        for bind in ["0.0.0.0:7700", "[::]:7700", "192.168.1.2:7700", "example.com:7700"] {
            assert!(loopback_bind(bind).is_err(), "{bind}");
        }
        assert_eq!(loopback_bind("localhost:8080").unwrap().to_string(), "127.0.0.1:8080");
        assert!(loopback_bind("[::1]:7700").is_ok());
        assert_eq!(display_url("127.0.0.1:8080".parse().unwrap()), "http://127.0.0.1:8080");
        assert_eq!(display_url("[::1]:8080".parse().unwrap()), "http://[::1]:8080");
    }

    #[cfg(unix)]
    #[test]
    fn setup_script_can_be_executed_from_a_fresh_checkout() {
        use std::os::unix::fs::PermissionsExt;
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("setup.sh");
        assert_ne!(std::fs::metadata(script).unwrap().permissions().mode() & 0o111, 0);
    }

    #[cfg(windows)]
    #[test]
    fn setup_handles_native_auth_stderr_on_windows_powershell_51() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("setup.ps1"), include_str!("../../setup.ps1")).unwrap();
        std::fs::write(temp.path().join("gh.cmd"), "@echo off\r\necho not authenticated 1>&2\r\nexit /b 1\r\n")
            .unwrap();
        std::fs::write(temp.path().join("git.cmd"), "@echo off\r\necho git mock\r\nexit /b 0\r\n").unwrap();
        std::fs::write(temp.path().join("claude.cmd"), "@echo off\r\nexit /b 0\r\n").unwrap();
        std::fs::write(temp.path().join("cargo.cmd"),
            "@echo off\r\nif \"%~1\"==\"--version\" (\r\n echo cargo mock\r\n exit /b 0\r\n)\r\necho %* > cargo-args.txt\r\nexit /b 1\r\n").unwrap();
        let powershell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut paths = vec![temp.path().to_path_buf()];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let output = std::process::Command::new(powershell)
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(temp.path().join("setup.ps1"))
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("LYRA_HOME", temp.path().join("lyra-home"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("gh is installed but not authenticated"), "{stdout}\n{stderr}");
        assert!(stdout.contains("Build failed"), "setup must continue after the auth diagnostic: {stdout}\n{stderr}");
        assert!(!stderr.contains("NativeCommandError"), "{stderr}");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("cargo-args.txt")).unwrap().trim(),
            "build --release --locked"
        );
        assert!(!temp.path().join("lyra-home").exists());
    }
}
