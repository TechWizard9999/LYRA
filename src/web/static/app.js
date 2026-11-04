// LYRA run page: live log over SSE. No framework; served from /static/app.js.
(function () {
  "use strict";
  var log = document.getElementById("log");
  if (!log || !window.EventSource) return;
  var runId = log.dataset.run;
  var renderedSeq = parseInt(log.dataset.seq || "0", 10);
  var LABELS = { intake: "S0 Intake", context: "S1 Context", repro: "S2 Repro", rca: "S3 RCA", fix: "S4 Fix", verify: "S5 Verify", pr: "S6 Draft PR" };
  var TERMINAL = { run_finished: 1, run_stopped: 1, stage_failed: 1, run_cancelled: 1, run_interrupted: 1 };

  function label(s) { return LABELS[s] || s || ""; }
  function firstLine(s) { return String(s || "").split("\n")[0]; }
  function secs(ms) { return (ms / 1000).toFixed(1) + "s"; }

  function describe(e) {
    switch (e.kind) {
      case "run_created": return ["info", "run created for " + e.request.issue];
      case "retry_requested": return ["info", "retry requested from " + label(e.from)];
      case "stage_started": return ["stage", "== " + label(e.stage) + " started (attempt " + e.attempt + ")"];
      case "stage_completed": return ["ok", "   " + label(e.stage) + " done in " + secs(e.duration_ms)];
      case "stage_failed": return ["err", "!! " + label(e.stage) + " failed: " + e.error.kind + ": " + firstLine(e.error.message)];
      case "run_stopped": return ["err", "STOPPED at " + label(e.stage) + " (" + e.reason + "): " + firstLine(e.message)];
      case "run_finished": return ["ok", "DONE " + (e.pr_url || "")];
      case "run_cancelled": return ["err", "CANCELLED" + (e.stage ? " at " + label(e.stage) : "")];
      case "run_interrupted": return ["err", "INTERRUPTED" + (e.stage ? " at " + label(e.stage) : "")];
      case "artifact_written": return ["muted", "+ " + e.path];
      case "log": return [e.level === "info" ? "muted" : "err", e.level + ": " + e.message];
      case "agent": {
        var a = e.event || {};
        switch (a.type) {
          case "tool_call": return ["tool", "[" + a.name + "] " + a.summary];
          case "text": return ["text", firstLine(a.text)];
          case "file_changed": return ["tool", "[edit] " + a.path];
          case "tool_result": return a.ok ? null : ["err", "[tool failed] " + firstLine(a.summary)];
          case "error": return ["err", "[error] " + a.message];
          case "finished": return [a.ok ? "muted" : "err", "[agent finished] " + firstLine(a.summary)];
          default: return null;
        }
      }
      default: return null;
    }
  }

  function setStage(stage, cls, text) {
    var el = document.getElementById("stage-" + stage);
    if (!el) return;
    el.className = "stage " + cls;
    el.querySelector(".status").textContent = text;
  }

  var es = new EventSource("/api/runs/" + encodeURIComponent(runId) + "/events?since=0");
  es.addEventListener("run", function (msg) {
    var e;
    try { e = JSON.parse(msg.data); } catch (_) { return; }
    var d = describe(e);
    if (d) {
      var stick = log.scrollTop + log.clientHeight >= log.scrollHeight - 20;
      var line = document.createElement("div");
      line.className = "line " + d[0];
      line.textContent = (e.ts || "").slice(11, 19) + "  " + d[1];
      log.appendChild(line);
      if (stick) log.scrollTop = log.scrollHeight;
    }
    if (e.seq <= renderedSeq) return; // replayed history: the page already reflects it
    if (e.kind === "stage_started") setStage(e.stage, "running", "running");
    if (e.kind === "stage_completed") setStage(e.stage, "done", "done");
    if (TERMINAL[e.kind]) { es.close(); setTimeout(function () { location.reload(); }, 400); }
  });
})();
