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

## 📡 Channels

The agent talks to the user through a pluggable channel layer.

| Channel   | Status                                          |
| --------- | ----------------------------------------------- |
| `tui`     | built-in ratatui terminal UI                    |
| `discord` | DM-only adapter, opt-in via `[channels.discord]`|

### Discord

DM-only with a runtime-mutable allowlist. The bot only responds to
direct messages from user ids you explicitly allow; guild channels
are ignored. Set up:

1. Create an application + bot at <https://discord.com/developers/applications>,
   enable the **MESSAGE CONTENT** privileged intent, copy the token.
2. Invite the bot to a server you share with it (Discord requires a
   shared guild before users can DM a bot).
3. Add the section to `~/.mandeven/mandeven.toml`:

   ```toml
   [channels.discord]
   enabled   = false   # boot-time auto-connect
   token_env = "DISCORD_BOT_TOKEN"
   ```

4. `export DISCORD_BOT_TOKEN=<your token>`, then run `mandeven`.

In the TUI:

| Command                       | Effect                                                     |
| ----------------------------- | ---------------------------------------------------------- |
| `/discord`                    | Toggle the gateway connection (off → on, on → off)         |
| `/discord status`             | Show enabled/disabled + allow-list count                   |
| `/discord allow <user_id>`    | Add a Discord user id to the allow list (persists)         |
| `/discord deny <user_id>`     | Remove a user id (persists)                                |
| `/discord list`               | Show current allow list                                    |
| `/discord autostart on\|off`  | Persist `enabled` in `mandeven.toml` for next launch       |

The allow list lives in `~/.mandeven/discord/allowlist.json`
(separate from `mandeven.toml` so runtime mutations don't churn the
config file). When the connection is open, the TUI top bar shows a
green `discord` badge.

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
