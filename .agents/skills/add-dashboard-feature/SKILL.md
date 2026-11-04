---
name: add-dashboard-feature
description: Add or change a dashboard page, JSON API endpoint, SSE behaviour, or the browser JavaScript under src/web/. Use for anything the user sees or calls on localhost.
---

# Add a dashboard feature

Stack: axum + maud + plain JS + SSE, one binary, **no JS build step**. Assets are compiled in with `include_str!` (`web/static/app.css`, `app.js`).

## Files
- `web/server.rs`: router, page handlers, request guard middleware.
- `web/api.rs`: JSON API handlers and `ApiError` (400 / 403 / 404 / 409).
- `web/sse.rs`: event stream, replay from `?since=N` or `Last-Event-ID`, `end` event when a run is terminal and inactive.
- `web/views.rs`: maud templates. `web/static/app.js` + `app.test.js`.
- API list: `POST /api/runs`, `GET /api/runs`, `GET /api/runs/{id}`, `GET /api/runs/{id}/events`, `POST /api/runs/{id}/cancel`, `POST /api/runs/{id}/retry/{stage}`, `GET /api/runs/{id}/artifacts/{path}`, `POST /api/doctor`, `GET /api/config`.

## Rules (see `.agents/rules/security.md`)
- Loopback only. Every request passes the Host check; every POST needs a matching `Origin`; honour `Sec-Fetch-Site`. New routes go behind the same middleware; do not add a route that bypasses it.
- `GET` must not launch subprocesses or mutate state. Anything that runs a process or changes a run is a guarded `POST`.
- Filesystem and git work goes through blocking workers (`spawn_blocking`), never on async request threads (see commit "dispatch dashboard cancellation through blocking workers").
- Validate ids and paths (run id before filesystem access; artifact paths reject symlinks/junctions). Serve artifacts as plain text with `nosniff`.
- Escape everything: maud escapes by default; in JS use `textContent`, never `innerHTML` with run data. Run data includes agent text and issue text.
- Error responses use the existing status codes and a clear message.

## Steps
1. Decide API shape first; keep JSON additive (new optional fields) so older clients/pages still work.
2. Handler + a test with `tower::ServiceExt::oneshot` and `http-body-util` against the router: success, bad input (400), foreign Origin/Host (403), missing run (404), conflict such as another run active (409).
3. View/JS change: keep it dependency-free. Add cases to `src/web/static/app.test.js` (`node --test src/web/static/app.test.js`).
4. SSE changes: test replay from a given `since`, reconnect with `Last-Event-ID`, and the `end` event on a terminal run. Clients must close their `EventSource` on `end`.
5. If you introduced a new event kind, status or stage state, that's a contract change (`change-contract`).
6. Look at it in a browser against a real or seeded run (`acceptance-run` shows how to start `lyra serve` with a scratch `LYRA_HOME`).
7. Update README's Dashboard API section and PLAN §6 (`update-docs`).

## Verify
`pre-pr-check` (includes the Node tests).
