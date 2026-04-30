# mandeven

**mandeven** is an agent for research work and everyday life written in Rust.

## 📦 Install

> [!NOTE]
> Prebuilt binaries are Apple Silicon macOS only. Other platforms
> build from source — anywhere a Rust 2024 toolchain runs.

```sh
curl -fsSL https://raw.githubusercontent.com/Si1w/mandeven/main/scripts/install.sh | sh
```

Or from source (installs into `~/.cargo/bin/`):

```sh
git clone https://github.com/Si1w/mandeven.git
cd mandeven
cargo install --path .
```

## 🚀 Quick start

**1. Set your provider API key**

| Provider   | Env var            |
| ---------- | ------------------ |
| Deepseek | `DEEPSEEK_API_KEY` |
| Mistral  | `MISTRAL_API_KEY`  |

**2. Launch**

```sh
mandeven
```

First launch walks an interactive prompt for provider, model, and
context window, then writes `~/.mandeven/mandeven.toml`. A typical
result looks like this — multiple providers and profiles can coexist;
the `default` key picks which one runs:

```toml
[llm]
default = "deepseek/deepseek-v4-flash"

[llm.mistral.mistral-small]
model_name         = "mistral-small-latest"
max_context_window = 256000

[llm.deepseek.deepseek-v4-flash]
model_name         = "deepseek-v4-flash"
max_context_window = 1000000

[agent.memory]
enabled = true
session_snapshot = true
profile_enabled = true
snapshot_limit = 8

[agent.dream]
enabled = true
schedule = "0 3 * * *"
run_on_startup = true
min_interval_secs = 72000
lock_stale_secs = 21600
min_sessions_per_run = 5
max_events_per_run = 80
max_prompt_chars = 24000
max_output_tokens = 2048
max_event_chars = 2000
max_existing_memories = 24
max_candidates = 8
```

**3. Chat**

Type into the composer. `/help` shows the slash-command panel.

## 🛠️ Tools

Registered automatically and advertised to the model on every turn.

| Capability | Covers                                                          |
| ---------- | --------------------------------------------------------------- |
| File       | Read regular UTF-8 files up to 5 MiB; write/edit inside the workspace; content search |
| Shell      | Run commands with read-only allow-listing or workspace-write deny-listing; not an OS sandbox |
| Web        | DuckDuckGo search and URL fetch with HTML→Markdown + SSRF guard |
| Task       | Markdown-backed progress ledger (create / update / list / get) for multi-step plans |
| Timer      | Markdown-backed schedules bound to task ids (create / update / list / delete / fire now) |

## 📡 Channels

The agent talks to the user through a pluggable channel layer.

| Channel   | Status                                          |
| --------- | ----------------------------------------------- |
| `tui`     | built-in ratatui terminal UI                    |
| `discord` | DM-only adapter, opt-in via `[channels.discord]`|
| `wechat`  | text-only personal WeChat iLink adapter with QR login, opt-in via `[channels.wechat]` |

## 🧩 Extra features

Optional subsystems are wired through `~/.mandeven/`. Runtime-mutable
state lives in sidecar files; durable enable/budget knobs live in
`mandeven.toml`.

| Feature     | Source                                | Effect                                                  |
| ----------- | ------------------------------------- | ------------------------------------------------------- |
| `skills`    | `~/.mandeven/skills/<name>/SKILL.md`  | Surfaced as `/<name>` slash commands + the `skill` tool |
| `hooks`     | `~/.mandeven/hooks.json`              | Shell commands fired on lifecycle events                |
| `timers`    | project bucket `timers/*.md`          | Scheduled tasks that re-enter the agent loop            |
| `exec`      | project bucket `execution/*.jsonl`    | Machine-readable history for scheduled task executions  |
| `cron`      | `~/.mandeven/cron/jobs.json`          | Compatibility scheduler for existing `/cron` jobs       |
| `heartbeat` | `[agent.heartbeat]` in `mandeven.toml`| Periodic self-check that can queue follow-up prompts    |
| `memory`    | `memory/*.md` + `[agent.memory]`      | Durable memories + frozen per-session prompt snapshot   |
| `dream`     | `[agent.dream]` in `mandeven.toml`    | Cron-driven background review that distills session evidence into global memory |

## Architecture notes

The target primitive-runtime design is documented in
[`docs/primitive-runtime-design.md`](docs/primitive-runtime-design.md). It
describes the intended direction: validated primitive tools as the RISC
instruction set, shell and skills as CISC escape hatches, Markdown for
user-visible state, and JSONL for execution history.

## 📜 License

Apache License 2.0. See [LICENSE](LICENSE).
