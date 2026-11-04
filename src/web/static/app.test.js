// Offline browser-behavior checks: node --test src/web/static/app.test.js
const test = require("node:test");
const assert = require("node:assert/strict");
const vm = require("node:vm");
const fs = require("node:fs");
const path = require("node:path");
const source = fs.readFileSync(path.join(__dirname, "app.js"), "utf8");

function page({ seq = 0, terminal = false, hasLog = true, artifactPaths = [] } = {}) {
  const handlers = {};
  const form = { dataset: {}, addEventListener(name, f) { handlers[name] = f; } };
  const buttons = [
    { tagName: "BUTTON", dataset: {}, disabled: false },
    { tagName: "BUTTON", dataset: {}, disabled: true },
  ];
  const windowHandlers = {};
  const events = {};
  const log = {
    dataset: { run: "r1", seq: String(seq), terminal: String(terminal) },
    scrollTop: 0, clientHeight: 100, scrollHeight: 100, lines: [],
    appendChild(line) { this.lines.push(line); },
  };
  const stage = { className: "", status: { textContent: "" }, querySelector() { return this.status; } };
  const artifacts = {
    items: artifactPaths.map((artifact) => ({ dataset: { artifact } })),
    querySelectorAll() { return this.items; },
    appendChild(item) { this.items.push(item); },
  };
  const emptyArtifacts = { hidden: artifactPaths.length > 0 };
  let closed = 0;
  let reloads = 0;
  function EventSource() {
    this.addEventListener = (name, f) => { events[name] = f; };
    this.close = () => { closed++; };
  }
  vm.runInNewContext(source, {
    window: { EventSource, addEventListener(name, f) { windowHandlers[name] = f; } }, EventSource,
    document: {
      getElementById(id) {
        if (id === "log") return hasLog ? log : null;
        if (id === "artifacts") return artifacts;
        if (id === "no-artifacts") return emptyArtifacts;
        return stage;
      },
      createElement() { return { dataset: {}, children: [], appendChild(child) { this.children.push(child); } }; },
      querySelectorAll(selector) {
        if (selector.startsWith("form")) return [form];
        if (selector.startsWith("button")) return buttons;
        return [form, ...buttons].filter((el) => el.dataset.submitting);
      },
    },
    location: { reload() { reloads++; } },
    setTimeout(f) { f(); },
  });
  return {
    form, buttons, handlers, windowHandlers, log, stage, artifacts, emptyArtifacts,
    event(value) { events.run({ data: JSON.stringify(value) }); },
    end() { events.end(); },
    get closed() { return closed; }, get reloads() { return reloads; },
  };
}

test("home form disables submissions and restores only its own disabled buttons", () => {
  const p = page({ hasLog: false });
  let prevented = 0;
  p.handlers.submit({ preventDefault() { prevented++; } });
  assert.equal(p.buttons[0].disabled, true);
  p.handlers.submit({ preventDefault() { prevented++; } });
  assert.equal(prevented, 1);
  p.windowHandlers.pageshow();
  assert.equal(p.buttons[0].disabled, false);
  assert.equal(p.buttons[1].disabled, true);
});

test("reconnected event delivery does not duplicate log lines", () => {
  const p = page({ seq: 10 });
  const event = { seq: 3, kind: "log", level: "info", message: "hello" };
  p.event(event); p.event(event);
  assert.equal(p.log.lines.length, 1);
});

test("old terminal events do not stop a retried run's live stream", () => {
  const p = page({ seq: 10 });
  p.event({ seq: 4, kind: "run_finished", pr_url: "https://example.org/pr/1" });
  assert.equal(p.closed, 0);
  p.event({ seq: 11, kind: "stage_started", stage: "fix", attempt: 2 });
  assert.equal(p.stage.status.textContent, "running");
  p.event({ seq: 12, kind: "stage_failed", stage: "fix", error: { kind: "guard", message: "failed" } });
  assert.equal(p.reloads, 1);
});

test("end closes terminal replay without a reconnect or reload loop", () => {
  const p = page({ seq: 5, terminal: true });
  p.event({ seq: 5, kind: "run_finished" });
  p.end();
  assert.equal(p.closed, 1);
  assert.equal(p.reloads, 0);
});

test("completion racing rendered state still reloads on end", () => {
  const p = page({ seq: 5, terminal: false });
  p.event({ seq: 5, kind: "run_finished" });
  p.end();
  assert.equal(p.reloads, 1);
});

test("a retry newer than a terminal page refreshes its controls", () => {
  const p = page({ seq: 5, terminal: true });
  p.event({ seq: 6, kind: "retry_requested", from: "fix" });
  assert.equal(p.reloads, 1);
});

test("new artifacts including policy become readable without reloading the running page", () => {
  const p = page({ seq: 1 });
  p.event({ seq: 2, kind: "artifact_written", path: "policy.md" });
  p.event({ seq: 3, kind: "artifact_written", path: "10-context/summary.json" });
  assert.equal(p.artifacts.items.length, 2);
  assert.equal(p.emptyArtifacts.hidden, true);
  assert.equal(p.artifacts.items[0].children[0].href, "/runs/r1/artifacts/policy.md");
  assert.equal(p.artifacts.items[1].children[0].textContent, "10-context/summary.json");
  assert.equal(p.reloads, 0);
});

test("artifact replay and repeated writes do not duplicate the rendered file list", () => {
  const p = page({ seq: 10, artifactPaths: ["00-issue.md"] });
  p.event({ seq: 2, kind: "artifact_written", path: "00-issue.md" });
  p.event({ seq: 3, kind: "artifact_written", path: "40-fix/archived-notes.md" });
  assert.equal(p.artifacts.items.length, 1, "old attempts must not resurrect archived files");
  p.event({ seq: 11, kind: "artifact_written", path: "20-repro/output.txt" });
  p.event({ seq: 11, kind: "artifact_written", path: "20-repro/output.txt" });
  p.event({ seq: 12, kind: "artifact_written", path: "20-repro/output.txt" });
  assert.equal(p.artifacts.items.length, 2);
  assert.equal(p.artifacts.items[1].children[0].href, "/runs/r1/artifacts/20-repro/output.txt");
  assert.equal(p.reloads, 0);
});

test("live artifact links keep text literal, encode URL characters and reject traversal", () => {
  const p = page();
  const name = '20-repro/<img src=x onerror=alert(1)>?#.txt';
  p.event({ seq: 1, kind: "artifact_written", path: name });
  const link = p.artifacts.items[0].children[0];
  assert.equal(link.textContent, name);
  assert.equal(link.href, "/runs/r1/artifacts/20-repro/" + encodeURIComponent(name.split("/")[1]));
  assert.equal(link.rel, "noopener");
  const invalidPaths = ["../run.json", "20-repro\\..\\run.json", "https://evil.example/x", "C:\\secret", "x\0y", ""];
  for (const [index, path] of invalidPaths.entries()) {
    p.event({ seq: index + 2, kind: "artifact_written", path });
  }
  assert.equal(p.artifacts.items.length, 1);
});
