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
```

**3. Chat**

Type into the composer. `/help` shows the slash-command panel.

## 🛠️ Tools

Registered automatically and advertised to the model on every turn.

| Capability | Covers                                                          |
| ---------- | --------------------------------------------------------------- |
| File       | Read, write, edit, and content search                           |
| Shell      | Run shell commands under the active sandbox tier                |
| Web        | DuckDuckGo search and URL fetch with HTML→Markdown + SSRF guard |

## 📡 Channels

The agent talks to the user through a pluggable channel layer.
One channel ships today; the registry is designed to grow.

| Channel | Status |
| ------- | ------ |
| `tui`   | built-in ratatui terminal UI |

## 🧩 Extra features

Opt-in subsystems wired through `~/.mandeven/`. Each is off until you
drop the matching file or flip the switch in `mandeven.toml`.

| Feature     | Source                                | Effect                                                  |
| ----------- | ------------------------------------- | ------------------------------------------------------- |
| `skills`    | `~/.mandeven/skills/<name>/SKILL.md`  | Surfaced as `/<name>` slash commands + the `skill` tool |
| `hooks`     | `~/.mandeven/hooks.json`              | Shell commands fired on lifecycle events                |
| `cron`      | `~/.mandeven/cron/jobs.json`          | Cron-scheduled prompts that re-enter the agent loop     |
| `heartbeat` | `[agent.heartbeat]` in `mandeven.toml`| Periodic self-check that can queue follow-up prompts    |

## 📜 License

Apache License 2.0. See [LICENSE](LICENSE).
