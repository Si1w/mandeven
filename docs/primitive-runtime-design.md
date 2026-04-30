# Primitive Runtime Design

This document describes the target architecture for collapsing cron,
heartbeat, memory, and task-like features into a smaller agent runtime.

The core rule is:

> RISC primitive tools are the small validated instruction set. `shell.exec`
> and `skill.use` are CISC escape hatches: shell executes a command language
> and returns output, while skills import workflow text. The runtime validates
> RISC instruction contracts and the execution envelope for CISC escapes, not
> shell command semantics or skill workflows.

## Goals

- Keep the model-facing primitive tool set small and orthogonal.
- Represent user-visible state as Markdown that users can read and edit.
- Represent machine execution history as JSONL.
- Treat sessions as a separate structured store.
- Make cron, heartbeat, memory, reminders, and routines derived capabilities
  instead of separate model-facing tool families.
- Treat shell as an explicit CISC execution escape hatch whose output is an
  observation.
- Keep skill workflows explicit: `skill.use` imports workflow text; it does not
  validate, plan, or execute the workflow.

## Storage Contract

Markdown is for user-visible state and user-visible deliverables.

JSONL is for machine-readable execution streams, runtime logs, and audit trails.

Session history is the exception. It should stay in its own structured store
because messages, tool calls, token usage, compaction state, and replay state are
not naturally editable Markdown.

Target layout:

```text
~/.mandeven/
  tasks/
    daily-paper-progress.md
    remind-me-to-check-build.md

  timers/
    daily-paper-progress-at-9am.md
    check-build-tomorrow.md

  memory/
    profile.md
    facts/
      prefers-short-answers.md
    projects/
      mandeven-runtime-design.md

  routines/
    heartbeat.md

  runs/
    <run_uuid>.jsonl

  logs/
    scheduler.jsonl
    watchdog.jsonl

  sessions/
    state.db
```

Markdown filenames are for humans, so they should be title slugs. Stable
machine identity lives in Markdown front matter as a UUID v7. New state files
use the common `---` front matter delimiter with YAML front matter.

References between runtime objects should use UUIDs, not filenames. This lets a
user rename `daily-paper-progress.md` without breaking the timer that points to
the task.

If a title slug conflicts, append a small readable suffix:

```text
daily-paper-progress.md
daily-paper-progress-2.md
```

Do not put UUIDs in Markdown filenames unless the user-facing title itself
contains one.

## Markdown State Specs

Task Markdown is declarative state: it says what the agent may execute.

```markdown
---
id: "0190b8e2-7a2c-7c40-a8d0-8a6a6f6f5c01"
kind: "task"
enabled: true
status: "pending"
created_at: "2026-04-30T12:00:00Z"
updated_at: "2026-04-30T12:00:00Z"
---

# Daily Paper Progress

Summarize recent paper progress and report blockers.
```

Timer Markdown is declarative state: it says why and when a task should run.

```markdown
---
id: "0190b8e3-1b4e-7a20-b991-2ad25fd7d301"
kind: "timer"
enabled: true
task_id: "0190b8e2-7a2c-7c40-a8d0-8a6a6f6f5c01"
schedule:
  kind: cron
  expr: "0 9 * * *"
next_fire_at: "2026-05-01T09:00:00Z"
created_at: "2026-04-30T12:01:00Z"
updated_at: "2026-04-30T12:01:00Z"
---

# Daily 9am Timer
```

The front matter is the machine-readable contract. The Markdown body is for
human context, instructions, and review.

## Tool ISA

The model-facing tool set has two layers.

RISC primitives:

```text
file.read
file.write
file.edit

web.search
web.fetch

task.create
task.update
task.list
task.get
task.run

timer.create
timer.update
timer.delete
timer.list
timer.fire_now
```

CISC escape hatches:

```text
shell.exec
skill.use
```

Only the RISC primitives are semantically validated instruction boundaries.

Examples:

- `file.*` validates path boundaries, encodings, write permissions, and patch
  shape.
- `web.*` validates URL and network safety policy.
- `task.*` validates task Markdown front matter and body shape.
- `timer.*` validates timer Markdown, schedule syntax, referenced `task_id`,
  and computed `next_fire_at`.

`shell.exec` is different. It validates the execution envelope: command policy,
working directory, timeout, and sandbox policy. It does not normalize or
validate the command's internal semantics.

`skill.use` is also different. It is a CISC workflow include, not a primitive
execution boundary.

## Shell Boundary

Shell is CISC because a shell command can encode arbitrary programs, pipelines,
scripts, package-manager calls, and interpreter invocations. It should remain
available because many operating-system capabilities are only exposed as
commands, but it should be treated as an execution escape hatch rather than a
RISC primitive.

`shell.exec(command)` should:

1. Validate the execution envelope: cwd, timeout, sandbox, command policy, and
   approval requirements.
2. Execute the command.
3. Return stdout, stderr, exit status, and timeout/interruption status as an
   execution observation.

It should not:

- parse the command into a normalized spec;
- infer durable state changes beyond the command result;
- hide output behind a higher-level semantic result;
- be used when a more precise RISC primitive exists.

The output itself is the observation:

```json
{
  "ok": true,
  "observation_type": "execution",
  "object": "shell",
  "exit_code": 0,
  "stdout": "tests passed\n",
  "stderr": ""
}
```

## Skill Boundary

Skills are the CISC layer.

`skill.use(name)` should:

1. Resolve the named skill.
2. Return or inject the raw `SKILL.md` workflow text for the agent to read.

It should not:

- validate the workflow;
- produce a normalized spec;
- plan the next steps;
- execute hidden side effects;
- rewrite the workflow into primitive operations.

Any workflow rules belong inside the skill text. The agent is responsible for
reading those rules and deciding which primitive tool calls to make next.

The only errors `skill.use` should surface are resource-resolution errors:

```text
skill not found
ambiguous skill name
skill file unreadable
```

The execution trace remains transparent:

```text
skill.use("paper-writing")
file.read(...)
file.edit(...)
```

not:

```text
skill.paper_write_and_edit_everything(...)
```

## Observations

Every tool call returns an observation. Observations fall into two families.

State observations report that a declarative object was created, updated,
parsed, or validated.

```json
{
  "ok": true,
  "observation_type": "state",
  "object": "timer",
  "id": "0190b8e3-1b4e-7a20-b991-2ad25fd7d301",
  "path": "timers/daily-paper-progress-at-9am.md",
  "validated": true,
  "diagnostics": [],
  "spec": {
    "task_id": "0190b8e2-7a2c-7c40-a8d0-8a6a6f6f5c01",
    "schedule": { "kind": "cron", "expr": "0 9 * * *" },
    "next_fire_at": "2026-05-01T09:00:00Z"
  }
}
```

Execution observations report direct results. `task.run` returns task output.
`shell.exec` returns process output.

```json
{
  "ok": true,
  "observation_type": "execution",
  "object": "task_run",
  "run_id": "0190b8e4-2c77-71e0-b51f-d7e4a7a102a9",
  "task_id": "0190b8e2-7a2c-7c40-a8d0-8a6a6f6f5c01",
  "status": "succeeded",
  "output": "Today's paper progress: ..."
}
```

Do not return run log paths as user-facing artifacts by default. A run log is
machine history. It belongs in JSONL and is read through explicit run/log tools
when needed.

Only return Markdown document paths when the run intentionally creates a
user-visible deliverable, such as:

```text
reports/weekly-paper-summary-2026-05-01.md
```

## Run History

Runs are execution streams, so their canonical record is JSONL:

```jsonl
{"type":"run_started","run_id":"0190...","task_id":"0190...","at":"2026-05-01T09:00:01Z"}
{"type":"tool_call","name":"web.search","args":{"query":"..."}}
{"type":"tool_result","name":"web.search","ok":true,"output":[...]}
{"type":"tool_call","name":"file.edit","args":{"path":"..."}}
{"type":"tool_result","name":"file.edit","ok":true,"observation":{"validated":true}}
{"type":"final_output","content":"Today's paper progress: ..."}
{"type":"run_finished","status":"succeeded","at":"2026-05-01T09:02:14Z"}
```

`task.run` returns the final execution output as its observation. The JSONL log
is for audit, debugging, and future agent reads, not for normal user-facing
presentation.

## Derived Capabilities

Higher-level features should compile down to primitive state and execution
operations.

Cron-style scheduled work:

```text
task.create(...)
timer.create(task_id, schedule={ kind: "cron", expr: "0 9 * * *" })
```

One-shot reminder:

```text
task.create(...)
timer.create(task_id, schedule={ kind: "at", at: "2026-05-01T14:00:00Z" })
```

Semantic heartbeat:

```text
tasks/heartbeat.md
timers/heartbeat-every-30m.md
routines/heartbeat.md
```

The heartbeat task can read `routines/heartbeat.md` and decide what to do.
There is no separate model-facing heartbeat tool.

Activity heartbeat:

```text
watchdog touch events
runs/<run_uuid>.jsonl
logs/watchdog.jsonl
```

This is runtime liveness, not user-facing task state.

Memory:

```text
memory/*.md
file.read
file.edit
optional memory Markdown validation
```

There is no primitive `memory.save` instruction. Durable memory is Markdown
state plus the file tools and validators.

## Runtime Components

Scheduler:

- scans `timers/*.md`;
- parses front matter;
- validates timer specs;
- finds due timers;
- advances `next_fire_at` atomically before enqueueing or starting a run;
- invokes `task.run(task_id)`;
- appends scheduler events to `logs/scheduler.jsonl`.

Runner:

- reads a validated task spec;
- constructs the agent input;
- executes the task through the normal tool loop;
- writes run JSONL;
- returns output as the execution observation.

Current implementation status:

- task and timer state are Markdown-backed;
- `TimerEngine` scans `timers/*.md`, advances due timers, and routes the
  referenced task through the normal agent iteration loop;
- explicit `task.run` and run JSONL are the next layer to split out of the
  agent iteration path.

Watchdog:

- observes execution liveness;
- records activity touches in JSONL;
- can time out or interrupt a run;
- never creates user-facing task state.

Validators:

- validate primitive state and runtime contracts;
- return diagnostics to the agent;
- do not own intelligence or workflow planning.

## Design Invariants

- Tools are the instruction set.
- RISC primitives are semantically validated by the runtime.
- Shell is a CISC execution escape hatch; only its execution envelope is
  validated.
- Shell output is a direct execution observation.
- Skills are CISC workflow text, not hidden execution.
- Markdown filenames are human-readable title slugs.
- UUID v7 front matter is stable machine identity.
- Markdown is for user-visible state and deliverables.
- JSONL is for execution history.
- Sessions remain structured storage.
- Cron, heartbeat, memory, reminders, and routines are derived capabilities.
- The runtime validates, schedules, records, and observes.
- The agent composes.
