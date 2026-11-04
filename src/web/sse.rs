//! `GET /api/runs/{id}/events?since=N`: replays `events.jsonl` after `since`, then streams
//! live events. SSE `id` = event seq (so a reconnecting `EventSource` resumes through
//! `Last-Event-ID`), SSE `event` = `run`, data = the `Event` JSON.

use super::api::{blocking, load_run, ApiResult};
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
    let engine = s.engine.clone();
    let check_id = id.clone();
    blocking(move || load_run(&engine, &check_id)).await?;
    let last_event_id =
        headers.get("last-event-id").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    let since = q.since.unwrap_or(0).max(last_event_id.unwrap_or(0));
    let stream = run_events(s.engine.clone(), id, since)
        .map(|ev| Ok(to_sse(&ev)))
        // EventSource otherwise reconnects forever after a terminal run's replay.
        .chain(stream::once(async { Ok(SseEvent::default().event("end").data("{}")) }));
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
    initialized: bool,
}

/// Replay-then-live, without gaps or duplicates: subscribe first, then read the history,
/// and drop live events whose seq was already replayed. If the receiver lags, the missed
/// events are re-read from `events.jsonl`.
pub fn run_events(engine: Arc<Engine>, id: String, since: u64) -> impl Stream<Item = Event> {
    let state = Live { engine, id, rx: None, last: since, buf: VecDeque::new(), initialized: false };
    stream::unfold(state, |mut st| async move {
        if !st.initialized {
            let engine = st.engine.clone();
            let id = st.id.clone();
            let since = st.last;
            let (rx, history) = blocking(move || {
                let rx = engine.subscribe(&id);
                Ok((rx, engine.history(&id, since)))
            })
            .await
            .ok()?;
            st.rx = rx;
            st.buf = history.into();
            st.initialized = true;
        }
        loop {
            if let Some(ev) = st.buf.pop_front() {
                st.last = st.last.max(ev.seq);
                return Some((ev, st));
            }
            let engine = st.engine.clone();
            let id = st.id.clone();
            let last = st.last;
            let (terminal, history) = blocking(move || {
                let terminal =
                    engine.get(&id).map(|r| r.status.is_terminal()).unwrap_or(true) && !engine.is_active(&id);
                // Once inactive, a final history read closes the save/event race.
                let history = if terminal { engine.history(&id, last) } else { Vec::new() };
                Ok((terminal, history))
            })
            .await
            .ok()?;
            if terminal {
                if history.is_empty() {
                    return None;
                }
                st.buf = history.into();
                continue;
            }
            let rx = st.rx.as_mut()?;
            let result = tokio::select! {
                event = rx.recv() => event,
                _ = tokio::time::sleep(Duration::from_millis(250)) => continue,
            };
            match result {
                Ok(ev) if ev.seq <= st.last => continue,
                Ok(ev) => {
                    st.last = ev.seq;
                    return Some((ev, st));
                }
                Err(RecvError::Lagged(_)) => {
                    let engine = st.engine.clone();
                    let id = st.id.clone();
                    let last = st.last;
                    st.buf = blocking(move || Ok(engine.history(&id, last))).await.ok()?.into();
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
        let rd = engine.run_dir("r1").unwrap();
        rd.create().unwrap();
        let sink = EventSink::open("r1", &events_path(&rd.root)).unwrap();
        sink.info("one");
        sink.emit(EventKind::StageStarted { stage: StageId::Intake, attempt: 1 });
        sink.info("three");
        let got: Vec<u64> = run_events(engine.clone(), "r1".into(), 1).take(2).map(|e| e.seq).collect().await;
        assert_eq!(got, vec![2, 3]);
    }
}
