//! `GET /api/runs/{id}/events?since=N`: replays `events.jsonl` after `since`, then streams
//! live events. SSE `id` = event seq (so a reconnecting `EventSource` resumes through
//! `Last-Event-ID`), SSE `event` = `run`, data = the `Event` JSON.

use super::api::{load_run, ApiResult};
use super::server::AppState;
use crate::engine::Engine;
use crate::events::Event;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures::stream::{self, Stream, StreamExt};
use serde::Deserialize;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::{error::RecvError, Receiver};

#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    #[serde(default)]
    pub since: Option<u64>,
}

pub async fn events(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    load_run(&s.engine, &id)?;
    let last_event_id =
        headers.get("last-event-id").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    let since = q.since.unwrap_or(0).max(last_event_id.unwrap_or(0));
    let stream = run_events(s.engine.clone(), id, since).map(|ev| Ok(to_sse(&ev)));
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

fn to_sse(ev: &Event) -> SseEvent {
    let data = serde_json::to_string(ev).unwrap_or_else(|_| "{}".into());
    SseEvent::default().id(ev.seq.to_string()).event("run").data(data)
}

struct Live {
    engine: Arc<Engine>,
    id: String,
    rx: Option<Receiver<Event>>,
    last: u64,
    buf: VecDeque<Event>,
}

/// Replay-then-live, without gaps or duplicates: subscribe first, then read the history,
/// and drop live events whose seq was already replayed. If the receiver lags, the missed
/// events are re-read from `events.jsonl`.
pub fn run_events(engine: Arc<Engine>, id: String, since: u64) -> impl Stream<Item = Event> {
    let rx = engine.subscribe(&id);
    let buf: VecDeque<Event> = engine.history(&id, since).into();
    let last = buf.back().map(|e| e.seq).unwrap_or(since);
    let state = Live { engine, id, rx, last, buf };
    stream::unfold(state, |mut st| async move {
        loop {
            if let Some(ev) = st.buf.pop_front() {
                st.last = st.last.max(ev.seq);
                return Some((ev, st));
            }
            let rx = st.rx.as_mut()?;
            match rx.recv().await {
                Ok(ev) if ev.seq <= st.last => continue,
                Ok(ev) => {
                    st.last = ev.seq;
                    return Some((ev, st));
                }
                Err(RecvError::Lagged(_)) => {
                    st.buf = st.engine.history(&st.id, st.last).into();
                }
                Err(RecvError::Closed) => return None,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::events::{events_path, EventKind, EventSink, StageId};

    #[tokio::test]
    async fn replays_history_after_since() {
        let home = tempfile::tempdir().unwrap();
        let engine = Engine::new(home.path().to_path_buf(), Config::default());
        let rd = engine.run_dir("r1");
        rd.create().unwrap();
        let sink = EventSink::open("r1", &events_path(&rd.root)).unwrap();
        sink.info("one");
        sink.emit(EventKind::StageStarted { stage: StageId::Intake, attempt: 1 });
        sink.info("three");
        let got: Vec<u64> = run_events(engine.clone(), "r1".into(), 1).take(2).map(|e| e.seq).collect().await;
        assert_eq!(got, vec![2, 3]);
    }
}
