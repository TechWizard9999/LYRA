//! Router, shared state, HTML page handlers and the compiled-in static assets.

use super::api::{self, load_run, ApiError};
use super::sse;
use super::views::{self, HomeView, NewRunForm, RunView};
use crate::engine::Engine;
use crate::state::RunRequest;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

const APP_CSS: &str = include_str!("static/app.css");
const APP_JS: &str = include_str!("static/app.js");

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        // HTML
        .route("/", get(home))
        .route("/runs", post(create_run_form))
        .route("/runs/{id}", get(run_page))
        .route("/runs/{id}/cancel", post(cancel_form))
        .route("/runs/{id}/retry/{stage}", post(retry_form))
        .route("/runs/{id}/artifacts/{*path}", get(api::artifact))
        .route("/static/app.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS) }))
        .route("/static/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS) }))
        // JSON API
        .route("/api/runs", post(api::create_run).get(api::list_runs))
        .route("/api/runs/{id}", get(api::get_run))
        .route("/api/runs/{id}/events", get(sse::events))
        .route("/api/runs/{id}/cancel", post(api::cancel_run))
        .route("/api/runs/{id}/retry/{stage}", post(api::retry_run))
        .route("/api/runs/{id}/artifacts/{*path}", get(api::artifact))
        .route("/api/doctor", get(api::doctor))
        .route("/api/config", get(api::config))
        .with_state(state)
}

fn render_home(engine: &Engine, form: &NewRunForm, error: Option<&str>) -> String {
    let runs = engine.list();
    let cfg = engine.config();
    let view = HomeView {
        runs: &runs,
        providers: cfg.providers.keys().cloned().collect(),
        default_provider: &cfg.run.provider,
        form,
        error,
        busy: engine.is_busy(),
    };
    views::home(&view).into_string()
}

async fn home(State(s): State<AppState>) -> Html<String> {
    Html(render_home(&s.engine, &NewRunForm::default(), None))
}

async fn create_run_form(State(s): State<AppState>, Form(f): Form<NewRunForm>) -> Response {
    let text = f.issue_text.trim();
    let req = RunRequest {
        repo: PathBuf::from(f.repo.trim()),
        issue: f.issue.clone(),
        issue_text: (!text.is_empty()).then(|| text.to_string()),
        provider: (!f.provider.is_empty()).then(|| f.provider.clone()),
        policy_acknowledged: f.policy_acknowledged.is_some(),
    };
    match api::submit(&s.engine, req) {
        Ok(id) => Redirect::to(&format!("/runs/{id}")).into_response(),
        Err(e) => (e.status, Html(render_home(&s.engine, &f, Some(&e.message)))).into_response(),
    }
}

#[derive(Debug, Default, Deserialize)]
struct RunPageQuery {
    #[serde(default)]
    error: Option<String>,
}

async fn run_page(State(s): State<AppState>, Path(id): Path<String>, Query(q): Query<RunPageQuery>) -> Response {
    let run = match load_run(&s.engine, &id) {
        Ok(r) => r,
        Err(e) => return html_error(e),
    };
    let rd = s.engine.run_dir(&id);
    let artifacts = rd.list();
    let policy = if run.has_policy || rd.exists("policy.md") { rd.read("policy.md").ok() } else { None };
    let last_seq = s.engine.history(&id, 0).last().map(|e| e.seq).unwrap_or(0);
    let view =
        RunView { run: &run, artifacts: &artifacts, policy: policy.as_deref(), last_seq, flash: q.error.as_deref() };
    Html(views::run_page(&view).into_string()).into_response()
}

async fn cancel_form(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    back_to_run(&id, api::cancel(&s.engine, &id).err())
}

async fn retry_form(State(s): State<AppState>, Path((id, stage)): Path<(String, String)>) -> Response {
    back_to_run(&id, api::retry(&s.engine, &id, &stage).err())
}

/// Redirects (303) to the run page, carrying an error message if the action failed.
fn back_to_run(id: &str, err: Option<ApiError>) -> Response {
    if !api::valid_run_id(id) {
        return Redirect::to("/").into_response();
    }
    match err {
        None => Redirect::to(&format!("/runs/{id}")).into_response(),
        Some(e) => Redirect::to(&format!("/runs/{id}?error={}", views::url_path(&e.message).replace('/', "%2F")))
            .into_response(),
    }
}

fn html_error(e: ApiError) -> Response {
    let body = views::layout(
        "Error",
        maud::html! {
            section.card {
                h2 { (e.status.as_u16()) " " (e.status.canonical_reason().unwrap_or("")) }
                p { (e.message) }
                p { a href="/" { "Back to runs" } }
            }
        },
    );
    (e.status, Html(body.into_string())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::events::{events_path, EventKind, EventSink, StageId};
    use crate::state::{RunState, Status};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app() -> (tempfile::TempDir, Arc<Engine>, Router) {
        let home = tempfile::tempdir().unwrap();
        let engine = Engine::new(home.path().to_path_buf(), Config::default());
        let app = router(AppState { engine: engine.clone() });
        (home, engine, app)
    }

    fn seed(engine: &Engine, id: &str) {
        let rd = engine.run_dir(id);
        rd.create().unwrap();
        let req = RunRequest {
            repo: PathBuf::from("."),
            issue: "#1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
        };
        let mut st = RunState::new(id.into(), req, "claude".into());
        st.status = Status::Failed;
        st.save(&rd.root).unwrap();
        rd.write("20-repro/output.txt", "boom").unwrap();
        let sink = EventSink::open(id, &events_path(&rd.root)).unwrap();
        sink.emit(EventKind::StageStarted { stage: StageId::Intake, attempt: 1 });
        sink.info("hello");
    }

    async fn call(app: &Router, method: &str, uri: &str) -> (StatusCode, String) {
        let req = Request::builder().method(method).uri(uri).body(Body::empty()).unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn list_runs_empty_home() {
        let (_h, _e, app) = app();
        let (status, body) = call(&app, "GET", "/api/runs").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "[]");
    }

    #[tokio::test]
    async fn unknown_run_is_404() {
        let (_h, _e, app) = app();
        let (status, body) = call(&app, "GET", "/api/runs/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("\"error\""), "{body}");
        let (status, _) = call(&app, "GET", "/runs/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(&app, "GET", "/api/runs/..").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn run_artifacts_and_pages() {
        let (_h, engine, app) = app();
        seed(&engine, "r1");
        let (status, body) = call(&app, "GET", "/api/runs/r1").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"id\":\"r1\""));
        let (status, body) = call(&app, "GET", "/api/runs/r1/artifacts/20-repro/output.txt").await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "boom"));
        let (status, _) = call(&app, "GET", "/api/runs/r1/artifacts/..%2Frun.json").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(&app, "GET", "/runs/r1/artifacts/missing.txt").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = call(&app, "GET", "/runs/r1").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("r1") && body.contains("S6 Draft PR") && body.contains(r#"data-seq="2""#));
        let (status, body) = call(&app, "GET", "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("/runs/r1"));
        let (status, _) = call(&app, "POST", "/api/runs/r1/retry/bogus").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(&app, "POST", "/api/runs/r1/cancel").await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, css) = call(&app, "GET", "/static/app.css").await;
        assert_eq!(status, StatusCode::OK);
        assert!(!css.is_empty());
    }

    #[tokio::test]
    async fn sse_replays_history() {
        let (_h, engine, app) = app();
        seed(&engine, "r1");
        let req = Request::builder().uri("/api/runs/r1/events?since=0").body(Body::empty()).unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/event-stream"));
        let mut body = res.into_body();
        let mut text = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !text.contains("id: 2") {
            let frame = tokio::time::timeout_at(deadline, body.frame()).await.expect("timed out").unwrap().unwrap();
            if let Ok(data) = frame.into_data() {
                text.push_str(&String::from_utf8_lossy(&data));
            }
        }
        assert!(text.contains("event: run"));
        assert!(text.contains("\"kind\":\"stage_started\""));
    }

    #[tokio::test]
    async fn form_errors_rerender_home() {
        let (_h, _e, app) = app();
        let req = Request::builder()
            .method("POST")
            .uri("/runs")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("repo=Z%3A%5Cdefinitely%5Cmissing&issue=%2342&provider=claude"))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned();
        assert!(body.contains("is not a directory"), "{body}");
        assert!(body.contains("value=\"#42\""), "form keeps values");
    }
}
