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
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use crate::engine::Engine;

/// Binds `bind`, prints the dashboard URL, optionally opens the browser and serves until
/// the process exits. Runs left `running` by a previous process are marked interrupted first.
pub async fn serve(engine: Arc<Engine>, bind: &str, open_browser: bool) -> anyhow::Result<()> {
    let interrupted = engine.mark_interrupted();
    if interrupted > 0 {
        println!("marked {interrupted} unfinished run(s) as interrupted (Retry is available on the run page)");
    }
    let app = router(AppState { engine });
    let listener = tokio::net::TcpListener::bind(bind).await.with_context(|| format!("binding {bind}"))?;
    let url = display_url(listener.local_addr()?);
    println!("LYRA dashboard: {url}");
    if open_browser {
        if let Err(e) = open::that_detached(&url) {
            eprintln!("could not open a browser ({e}); open {url} yourself");
        }
    }
    axum::serve(listener, app).await.context("serving the dashboard")?;
    Ok(())
}

/// The URL to show/open: an unspecified bind address (0.0.0.0 / ::) is reachable on localhost.
fn display_url(addr: SocketAddr) -> String {
    let ip = if addr.ip().is_unspecified() { IpAddr::V4(Ipv4Addr::LOCALHOST) } else { addr.ip() };
    format!("http://{}", SocketAddr::new(ip, addr.port()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_url_maps_unspecified_to_localhost() {
        assert_eq!(display_url("0.0.0.0:7700".parse().unwrap()), "http://127.0.0.1:7700");
        assert_eq!(display_url("127.0.0.1:8080".parse().unwrap()), "http://127.0.0.1:8080");
    }
}
