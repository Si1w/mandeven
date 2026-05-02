---
name: heartbeat
description: Periodically review standing reminders, routines, and follow-up checks; notify only when something needs attention.
user-invocable: true
allowed-tools: task_* timer_*
timers: "*/30 * * * *"
fork: true
---

Use this skill to perform a quiet heartbeat review.

Workflow:
1. Inspect relevant task and timer state for reminders, routines, overdue follow-ups, or checks that should happen now.
2. If there is nothing actionable, reply with exactly `[SILENT]`.
3. If something needs attention, write a concise reminder with the concrete item, why it needs attention now, and the next action.

Rules:
- Do not invent reminders that are not backed by task or timer state.
- Prefer silence over low-value status chatter.
- Keep notifications short enough to be useful as an ambient notice.
