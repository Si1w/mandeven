---
name: cron
description: Create, update, list, or run cron-like delayed and recurring work through task and timer state.
user-invocable: true
allowed-tools: task_* timer_*
---

# Cron

Use this skill when the user wants delayed, recurring, or cron-like autonomous work.

## Workflow

1. Clarify the intended schedule only when the user's request does not include enough timing detail.
2. Check existing tasks and timers before creating anything new.
3. For a new scheduled workflow, create or reuse a task that describes the work, then create a timer for that task.
4. For updates, change the existing timer or task instead of creating duplicates.
5. For immediate execution, fire the timer or run the referenced task, then report what happened.

## Rules

- Do not create scheduled work unless the user clearly asks for future, delayed, or recurring execution.
- Use `timer_*` for schedule state and `task_*` for the work to run.
- Prefer editing existing schedules over creating similar new ones.
- When confirming a schedule, include the schedule, whether it is enabled, and what task will run.
