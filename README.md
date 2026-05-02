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
max_bytes = 25000
max_lines = 200
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
| Task       | Markdown-backed work items (create / update / list / get / run) for multi-step plans and scheduled work |
| Timer      | JSON-backed `at` / `every` / `cron` schedules bound to task ids (create / update / list / delete / fire now) |

## 📡 Channels

The agent talks to the user through a pluggable channel layer.

| Channel   | Status                                          |
| --------- | ----------------------------------------------- |
| `tui`     | built-in ratatui terminal UI                    |
| `discord` | DM-only adapter, opt-in via `[channels.discord]`|
| `wechat`  | text-only personal WeChat iLink adapter with QR login, opt-in via `[channels.wechat]` |

## 🧩 Runtime state

These are backing stores for the tool instruction set and optional
subsystems, not separate tool categories. Runtime-mutable state lives in
sidecar files; durable enable/budget knobs live in `mandeven.toml`.

| State       | Source                                | Effect                                                  |
| ----------- | ------------------------------------- | ------------------------------------------------------- |
| `skills`    | built-in + `~/.mandeven/skills/<name>/SKILL.md` | Surfaced as `/<name>` slash commands + the `skill` tool |
| `hooks`     | `~/.mandeven/hooks.json`              | Shell commands fired on lifecycle events                |
| `tasks`     | project bucket `tasks/*.md`           | User-visible task state and explicit task executions    |
| `timers`    | `~/.mandeven/timers.json`             | Schedule triggers for tasks and skills                  |
| `exec`      | project bucket `execution/*.jsonl`    | Machine-readable history for task executions            |
| `memory`    | `~/.mandeven/MEMORY.md` + `[agent.memory]` | Durable user memory injected as transient user context |

## 📜 License

Apache License 2.0. See [LICENSE](LICENSE).
