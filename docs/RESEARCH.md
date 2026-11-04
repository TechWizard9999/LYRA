# LYRA — research notes and deferred designs

Companion to `PLAN.md`. Everything here is background or post‑MVP. Dates are as of 2026‑11‑04.

## 1. Deferred: multi‑model RCA

Original idea: NVIDIA → Codex → Claude, each reviewing the previous RCA, Claude deciding. The evidence (§4) says a sequential chain anchors later models on earlier answers, and collaborating models underperform their best member. The alternative kept for later:

- Run N RCAs **in parallel and blind**, one per runtime (Claude Code, Codex, Copilot CLI, OpenCode for NVIDIA or any OpenAI‑compatible key).
- **Isolation** (from the v3 review): each RCA gets its own disposable checkout pinned to the same commit, the same input evidence, and a private output directory; native read‑only modes; the git check is change detection only. Sharing one worktree is neither blind nor read‑only.
- Harness computes `agreement.json` (files/symbols named by all, by one only, confidence spread).
- One **adjudicator** reads the RCAs anonymised (A/B/C/D) plus the code; must cite file:line; prefers the mechanism that predicts the exact repro output; keeps disagreements visible. Agreement is a prior, not a verdict.
- Result must be `supported | unresolved`; a provider failure stops the run or is recorded explicitly, never hidden.
- Keep a sequential `chain` mode only when an eval set exists to compare them.
- Later still: N candidate fixes selected by executing the repro + regression tests first, an LLM verifier only on survivors (Agentless, Trae, OpenHands critic, LLM‑as‑a‑Verifier).

Other deferred runtimes, verified headless modes:

- Copilot CLI 1.0.93: `copilot -p "<prompt>" -C <dir> --add-dir <dir> --allow-all-tools --output-format json --model <m> --reasoning-effort high`; auth `COPILOT_GITHUB_TOKEN` > `GH_TOKEN` > `GITHUB_TOKEN`; deny rules beat allow rules; stdin ignored with `-p`.
- OpenCode: `opencode run --dir <dir> --model nvidia/<model> --format json --auto "<prompt>"`; NVIDIA is a built‑in provider (`NVIDIA_API_KEY`); any OpenAI‑compatible endpoint via a `provider` block with `"npm": "@ai-sdk/openai-compatible"` and `baseURL`; permissions default to allow; `--format json` event shape undocumented (pin the version); loads project config unless controlled; Windows needs `OPENCODE_GIT_BASH_PATH`.
- Generic `command` adapter: any executable, prompt on stdin.
- Direct HTTP adapters (Anthropic Messages API, OpenAI‑compatible chat completions with a small tool loop) for setups without a CLI. Codex's custom `model_providers` only speak the Responses wire API, so NVIDIA cannot be proxied through Codex.

Deferred harness features: cost display (Claude's `total_cost_usd` and `--max-budget-usd` are client‑side estimates that can overshoot; subscription usage is not in dollars), per‑stage time budgets, automatic crash resume, automatic fix‑retry loop alternating providers, eval set (`lyra eval` on closed issues with known fixes), web search in context, container sandbox, GitHub label/comment trigger, pre‑work issue comment, PR outcome tracking.

## 2. Existing systems (survey)

| System | Trigger | Pipeline | Multi‑model / verifier | Lang / headless | Fixed‑harness score | Notes |
|---|---|---|---|---|---|---|
| OpenHands (MIT, 92k★) | `openhands` label / `@openhands` | explore → edit → tests → PR; pushes branch even on failure | trajectory critic, best‑of‑N: 66.4% vs 60.6% (Apr 2025); 4B critic Best@8 73.8% vs 57.9% random, early stop at 1.35 attempts (Mar 2026) | Python; `--headless --json` (non‑JSON lines mixed in) | 66.4% (2025) | Docker per run |
| mini‑swe‑agent (MIT, 9k★) | CLI/batch | ~100‑line bash‑only loop | none | Python | ">74%"; official swebench.com harness | tiny tool surface works |
| Agentless (MIT) | CLI | hierarchical localise → sample N patches → filter by syntax + regression tests → generate repro tests → rerank | best‑of‑N by execution | Python | 50.8% (Dec 2024) | the recipe everyone reuses |
| Refact.ai (BSD, Rust engine) | IDE/CLI | AST+VecDB explore → write reproduction script → debug sub‑agent (≤3 calls) → one planning call → edit → rerun repro + tests; dropped the standalone critic ("tests beat critique") | multi‑model roles | Rust `refact-lsp` | 70.4% → 74.4% (2025) | closest to LYRA |
| Trae Agent (MIT, 12k★) | CLI | N runs → cluster equivalent patches → regression‑prune → LLM selector | ensemble | Python | 75.2% (Jul 2025) | selection after execution filter |
| Live‑SWE‑agent (UIUC) | research | mini‑swe‑agent + self‑built tools | none | Python | 77.4%; Pro 45.8% | top fixed‑harness entry late 2026 |
| Claude Code Action / web | `@claude`, label, assignee | branch, PR, progress checklist, `--json-schema` | none | TS; `claude -p` | – | |
| OpenAI Codex (Apache, 133k★) | cloud, GitHub/Linear/Slack, `codex cloud exec --attempts N` | per‑task workspace → PR | best‑of‑N in cloud | Rust (`codex-rs`); `codex exec --json` | – | reference Rust code |
| Copilot coding agent + CLI | assign issue / `@copilot`; Agent Tasks REST API | Actions session, PR + living checklist, iterates on review | – | closed; `copilot -p --output-format json` | – | 59‑min cap |
| Google Jules | label / API | VM, plan → approval, critic | critic | closed | – | shutting down 2026‑12‑31 |
| LangChain Open SWE (MIT, 11k★) | label / comments | Manager → Planner (approval) → Programmer + Reviewer loop → PR | reviewer loop | TS/Py, LangGraph | – | plan‑approval gate |
| goose (Apache, 56k★), Aider (50k★), Cline, Droid, Amp, Devin, Cursor | CLI/VM | general agents | Aider alternated GPT‑4o/Opus across retries | goose Rust | – | Droid: "exit 0 ≠ success" |

Leaderboards: swebench.com evaluates every model with mini‑swe‑agent and requires trajectories plus the best@k selection mechanism; SWE‑bench Pro public tops out ~60% (Nov 2026); vendor self‑reported numbers run 10–30 points above fixed‑harness results. Architecture of top results: parallel rollouts + a verifier (SWE‑Lego: sequential scaling saturates at ~100–140 turns; R2E‑Gym: execution‑based + LLM verifier hybrid; LLM‑as‑a‑Verifier: 3 trajectories × 3 models + verifier).

## 3. Boards, orchestrators and engines reviewed for reuse

| Project | What it is | Verdict |
|---|---|---|
| Vibe Kanban (BloopAI) | Rust/axum + React kanban for coding agents, worktrees, PRs, 10 executors, Apache‑2.0, 28k★ | Sunsetting banner on the repo (cloud wound down Apr 2026); no custom‑executor API |
| cezar (open‑mercato) | TypeScript control center: agents on GitHub issues, worktree per task, draft PRs, YAML workflows, HTTP API, MIT | Closest feature match, but TS and one agent per task; could drive LYRA via its API later |
| OpenAI Symphony | Apache‑2.0 spec + Elixir reference: Linear board → isolated Codex runs; "engineering preview"; no UI | Prior art for board‑as‑control‑plane |
| symphony‑code, sweo, chipping‑orchestrator | small TS/Go/Python pollers: GitHub issues → agent → PR, some with SSE dashboards | pattern confirmed, nothing to reuse in Rust |
| Restate | Rust durable‑execution server with built‑in UI/CLI; Rust SDK 0.10 ("may break between releases") | right model for multi‑machine later; adds a server now |
| Temporal | Rust SDK generally available since 2026‑09‑23 (`temporalio-sdk`) | viable later for distributed runs; needs a Temporal server |
| Dioxus 0.7 fullstack | Rust UI (WASM) with server functions, SSE, websockets | possible front end later over the same API |

## 4. Practitioner and maintainer evidence

- **Merge rates.** 9,799 human‑reviewed agent PRs (MSR'26): Codex 71%, Cursor 71%, Claude Code 61%, Copilot 59%, Devin 54%; only ~36% of rejections were clear agent failures. Bug fixes 66% vs chores 84%. dotnet/runtime, 10 months of Copilot: 68% merged vs 87% human, same revert rate; 1–10‑line PRs 80% vs 64% for 100–500 lines; success went 38% → 69% from `copilot-instructions.md` and build access ("better preparation, not better models"); review was the bottleneck.
- **Unattended bots in strangers' repos:** 240 drive‑by PRs → 30% merged, ~75% where the author had prior merges, ~0% elsewhere; a pre‑work comment tripled merges.
- **Policies (2026):** of 118 popular repos with an AI policy, 78% allow, 22% discourage, 51% require disclosure, 74% require a human in the loop. Bans: Zig, QEMU, NetBSD, Gentoo; GCC requires `Assisted-by:` and human‑only sign‑off; LLVM bans unattended agents and AI on `good first issue`; Linux kernel `Assisted-by: AGENT:MODEL`; Ghostty requires disclosure of tool and extent; curl: "if someone can spot that the contribution was made with the help of AI, you have more work to do". GitHub added repo settings to restrict PRs and per‑user open‑PR caps. → LYRA shows the repo's own policy text and follows it; no blanket rules.
- **Multi‑model:** 154 confirmed defects across 8 models / 5 providers: no model above ~64% recall, 56% of defects found by exactly one model, a second provider raised coverage 34% → 57%. Collaborating LLM teams underperform their best member by 8–38% ("integrative compromise"). Judges shown a prior score anchor on it (7 of 8 judges, CIKM '26). Claude+Codex both approved the same wrong fix: "agreement is a strong prior, not a verdict".
- **Reproduction first:** ORACLE‑SWE ranks the reproduction test as the most valuable input to a SWE agent (+24–28 points when a strong model writes it); 15.7% of "passing" SWE‑bench Verified patches were wrong under stronger tests (UTBoost); agents edit tests when stuck (Opus 4.1 kept a 46% cheat rate in ImpossibleBench; EvilGenie caught explicit reward hacking by Codex and Claude Code; test‑file‑edit detection worked best). → repro gate and test lock in PLAN.md §3.
- **Runtime gotchas:** Claude `-p` without `--bare` runs the repo's hooks and MCP servers with no trust prompt; `--bare` does not use the subscription login; `stream-json` needs `--verbose`; `-p` starts in Manual mode since v2.1.302; `--allowedTools` grants, `--tools`/`--disallowedTools` restrict; cost is a client estimate that can overshoot; Windows Git‑Bash regressions. Codex: read‑only sandbox by default, gate on `turn.failed`, Windows sandbox experimental. Copilot: deny beats allow, stdin ignored with `-p`. OpenCode: permissions default to allow, `--format json` undocumented.

## 5. Sources

- Runtimes: Claude Code headless https://code.claude.com/docs/en/headless · CLI reference https://code.claude.com/docs/en/cli-reference · structured outputs https://code.claude.com/docs/en/agent-sdk/structured-outputs · Codex non‑interactive https://learn.chatgpt.com/docs/non-interactive-mode · Codex config https://learn.chatgpt.com/docs/config-file/config-reference · Copilot CLI https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-programmatic-reference · OpenCode CLI https://opencode.ai/docs/cli/ · OpenCode providers https://opencode.ai/docs/providers/ · OpenCode config https://opencode.ai/docs/config/ · NVIDIA API https://docs.api.nvidia.com/nim/reference/llm-apis · GitHub sub‑issues https://docs.github.com/en/rest/issues/sub-issues
- Engines/UI: Restate https://github.com/restatedev/restate · restate‑sdk https://docs.rs/restate-sdk · Temporal Rust SDK GA https://temporal.io/blog/build-durable-applications-rust-temporal-rust-sdk-now-generally-available · Dioxus 0.7 https://dioxuslabs.com/learn/0.7/essentials/fullstack/
- Boards: Vibe Kanban https://github.com/BloopAI/vibe-kanban · cezar https://github.com/open-mercato/cezar · Symphony https://github.com/openai/symphony · symphony‑code https://github.com/nnndv/symphony-code · roundup https://www.augmentcode.com/tools/open-source-agent-orchestrators · https://github.com/bradagi/awesome-cli-coding-agents
- Systems: OpenHands https://github.com/All-Hands-AI/OpenHands , https://openhands.dev/blog/sota-on-swe-bench-verified-with-inference-time-scaling-and-critic-model , https://openhands.dev/blog/20260305-learning-to-verify-ai-generated-code · mini‑swe‑agent https://github.com/SWE-agent/mini-swe-agent · Agentless https://github.com/OpenAutoCoder/Agentless , https://arxiv.org/abs/2407.01489 · Refact https://refact.ai/blog/2025/1-agent-on-swe-bench-verified-using-claude-4-sonnet/ · Trae https://github.com/bytedance/trae-agent , https://arxiv.org/abs/2507.23370 · Live‑SWE‑agent https://arxiv.org/abs/2511.13646 · Claude Code Action https://github.com/anthropics/claude-code-action · Codex https://github.com/openai/codex · Copilot coding agent https://docs.github.com/en/copilot/concepts/agents/coding-agent/about-coding-agent · Jules https://jules.google/docs/changelog · Open SWE https://github.com/langchain-ai/open-swe · goose https://github.com/aaif-goose/goose · Aider https://aider.chat/2024/05/22/swe-bench-lite.html
- Leaderboards/verifiers: https://swebench.com/verified · https://labs.scale.com/leaderboard/swe_bench_pro_public · SWE‑Lego https://arxiv.org/abs/2601.01426 · R2E‑Gym https://arxiv.org/abs/2504.07164 · SWE‑World https://arxiv.org/abs/2602.03419 · LLM‑as‑a‑Verifier https://arxiv.org/abs/2607.05391
- Merge data: MSR'26 https://arxiv.org/abs/2605.22534 · AIDev https://arxiv.org/html/2602.08915 · dotnet/runtime https://devblogs.microsoft.com/dotnet/ten-months-with-cca-in-dotnet-runtime/ · 240‑PR experiment https://dev.to/zeroknowledge0x/i-sent-240-prs-to-open-source-repos-using-an-ai-agent-heres-the-brutal-truth-about-what-actually-16ic · Godot https://www.theregister.com/2026/02/18/godot_maintainers_struggle_with_draining/
- Policies: ICSME'26 https://arxiv.org/abs/2605.16706 · LF Research https://www.linuxfoundation.org/press/new-lf-research-survey-shows-generative-ai-cuts-open-source-coding-time-in-half-but-25-of-incoming-pull-requests-are-considered-ai-slop · Zig https://ziglang.org/code-of-conduct/ · QEMU https://www.qemu.org/docs/master/devel/code-provenance.html · GCC https://gcc.gnu.org/ai-policy.html · LLVM https://llvm.org/docs/AIToolPolicy.html · Ghostty https://raw.githubusercontent.com/ghostty-org/ghostty/main/AI_POLICY.md · curl https://raw.githubusercontent.com/curl/curl/master/docs/CONTRIBUTE.md · GitHub PR settings https://github.blog/changelog/2026-02-13-new-repository-settings-for-configuring-pull-request-access/
- Multi‑model: https://zenodo.org/records/20519585 · https://arxiv.org/abs/2602.01011 · anchoring https://arxiv.org/abs/2608.25869 · RCA failures https://dl.acm.org/doi/10.1145/3793655.3793732 · juries https://orq.ai/blog/llm-juries-in-practice · Claude+Codex week https://dev.to/brianmello/i-let-claude-and-codex-argue-about-my-code-for-a-week-heres-what-they-caught-gg0
- Reproduction/test hacking: ORACLE‑SWE https://arxiv.org/html/2604.07789v1 · UTBoost https://arxiv.org/abs/2506.09289 · ImpossibleBench https://arxiv.org/abs/2510.20270 · EvilGenie https://arxiv.org/abs/2511.21654 · CoHarden https://arxiv.org/abs/2607.19843 · SWT‑Bench https://arxiv.org/html/2406.12952v3
- Runtime gotchas: https://dev.to/gabrielanhaia/headless-mode-in-5-agents-claude-p-codex-exec-gemini-p-copilot-p-opencode-run-4816 · https://github.com/anthropics/claude-code/issues/53068
