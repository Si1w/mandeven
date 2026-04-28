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
| Task       | In-session progress ledger (create / update / list / get) for multi-step plans |
| Cron       | Manage scheduled prompts (create / list / delete); paired with the user-facing `/cron` command |
| Memory     | Durable cross-session memories with a derived user profile; paired with the user-facing `/memory` command |

## 📡 Channels

The agent talks to the user through a pluggable channel layer.

| Channel   | Status                                          |
| --------- | ----------------------------------------------- |
| `tui`     | built-in ratatui terminal UI                    |
| `discord` | DM-only adapter, opt-in via `[channels.discord]`|

## 🧩 Extra features

Optional subsystems are wired through `~/.mandeven/`. Runtime-mutable
state lives in sidecar files; durable enable/budget knobs live in
`mandeven.toml`.

| Feature     | Source                                | Effect                                                  |
| ----------- | ------------------------------------- | ------------------------------------------------------- |
| `skills`    | `~/.mandeven/skills/<name>/SKILL.md`  | Surfaced as `/<name>` slash commands + the `skill` tool |
| `hooks`     | `~/.mandeven/hooks.json`              | Shell commands fired on lifecycle events                |
| `cron`      | `~/.mandeven/cron/jobs.json`          | Cron-scheduled prompts that re-enter the agent loop     |
| `heartbeat` | `[agent.heartbeat]` in `mandeven.toml`| Periodic self-check that can queue follow-up prompts    |
| `memory`    | `[agent.memory]` in `mandeven.toml`   | Durable memories + frozen per-session prompt snapshot   |

## 📜 License

Apache License 2.0. See [LICENSE](LICENSE).
