// Offline browser-behavior checks: node --test src/web/static/app.test.js
const test = require("node:test");
const assert = require("node:assert/strict");
const vm = require("node:vm");
const fs = require("node:fs");
const path = require("node:path");
const source = fs.readFileSync(path.join(__dirname, "app.js"), "utf8");

function page({ seq = 0, terminal = false, hasLog = true } = {}) {
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
  let closed = 0;
  let reloads = 0;
  function EventSource() {
    this.addEventListener = (name, f) => { events[name] = f; };
    this.close = () => { closed++; };
  }
  vm.runInNewContext(source, {
    window: { EventSource, addEventListener(name, f) { windowHandlers[name] = f; } }, EventSource,
    document: {
      getElementById(id) { return id === "log" ? (hasLog ? log : null) : stage; },
      createElement() { return {}; },
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
    form, buttons, handlers, windowHandlers, log, stage,
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
