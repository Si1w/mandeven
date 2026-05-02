---
name: memorize
description: Update MEMORY.md when the user explicitly asks to remember, forget, or preserve a durable preference, correction, fact, or collaboration rule.
user-invocable: true
allowed-tools: file_read file_edit
---

# Memorize

Use this skill only when the user explicitly asks you to remember, forget, or preserve something durable across future sessions, or gives stable feedback about how you should work.

## Workflow

1. Read the `MEMORY.md` path shown in the user memory context.
2. Decide whether the request is durable enough to save.
3. Edit an existing bullet if it already covers the same fact.
4. Otherwise add one concise bullet under the closest heading.
5. If the user asks to forget something, remove or rewrite the matching bullet.

## Save

- User role, goals, long-term preferences, and stable collaboration style.
- Stable feedback about assistant behavior, including what to avoid or repeat.
- Durable project context that is not derivable from files, git, tasks, or timers.
- External reference pointers and why they matter.

## Do Not Save

- Secrets, credentials, tokens, private keys, passwords, or raw identifiers that grant access.
- Temporary task status, current conversation progress, or one-off reminders.
- Code structure, file paths, APIs, commands, or facts that can be read from the repo.
- Procedures or workflows; those belong in skills or AGENTS.md.
- Duplicate memories.

## Rules

- Keep `MEMORY.md` short, direct, and hand-editable.
- Prefer a single clear bullet over a paragraph.
- Convert relative dates into absolute dates when saving time-sensitive context.
- Do not mention memory updates unless the user asked for confirmation or the edit changes your answer.
