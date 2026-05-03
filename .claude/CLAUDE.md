## Path Hint

This file is the shared repository instruction source for Claude and Codex.

- Claude reads it from `.claude/CLAUDE.md`.
- Codex reads the repository-level `AGENTS.md`, which should be a symlink to
  this file.
- These rules apply to the whole repository.

## Agent Operating Rules

- Treat the worktree as shared. Do not revert, overwrite, or clean up changes
  you did not make unless the user explicitly asks for that operation.
- Keep edits focused on the user's request and follow the existing module
  structure under `src/`.
- Prefer `rg` and `rg --files` for searching.
- Do not create commits unless the user asks. When asked to commit, keep commits
  small and give them clear messages.

## Development Workflow

These principles govern *how* work gets done in this project. They apply to every feature, refactor, and non-trivial bug fix.

### Design First

- **Define data structures and types before logic.** Write the `struct`, `enum`, and trait signatures first. Forcing the data model out early forces the business logic to be thought through, and the interfaces fall out naturally.
- **Interface before implementation.** Nail down function signatures and module APIs, then fill in the bodies. Contract-first, TDD-adjacent.

### Naming & Structure

- **Self-documenting names.** `get_user_by_email()` beats `get_user2()` by a hundred. Long is fine; vague is not.
- **Single responsibility per function.** One function, one job. If it exceeds ~30 lines, question whether it should split.
- **Plan directory layout early.** Decide feature-organized vs. layer-organized up front — moving files later is expensive.

### Development Rhythm

- **Small, focused increments.** Keep each working increment coherent. If the user asks for commits, commit each increment with a clear message.
- **Happy path first, then edges.** Get the main flow working before chasing every edge case or error branch.
- **Tag `TODO` / `FIXME` consciously.** Mark temporary solutions explicitly — don't let them silently become permanent.

### Validation & Testing

- **Ship a runnable demo early.** The sooner it actually runs, the sooner you catch a wrong direction.
- **Unit-test the core logic.** Not 100% coverage — but key algorithms and data transformations must have tests.
- **A bug isn't fixed until you can reproduce it locally.** No guessing. Reproduce, then fix.
- **Run CI/CD-equivalent tests before committing.** Before creating a commit or PR, run the project tests that mirror CI/CD for the touched area. For this Rust repo, run `cargo test` unless a narrower documented CI command is clearly sufficient. Report the command and result in the final response.
- **Keep Clippy strict.** Run `cargo clippy --all-targets --all-features -- -W clippy::pedantic -D warnings` before closing out Rust changes.

### Defensive Mindset

- **Distrust external input.** API params, user input, third-party responses — validate at the boundary.
- **Error handling is not an afterthought.** For IO and network calls, design the failure path at the same time as the happy path.
- **Extract magic numbers into constants.** `const MAX_RETRY: u32 = 3;` beats a bare `3` scattered through the code.

### Documentation & Comments

- **Comments explain *why*, not *what*.** The code says what it does; comments explain the reasoning behind non-obvious choices.
- **Document counter-intuitive business logic.** If a reader would reasonably ask "why this way?", leave the answer in a comment.

---

## Core Rule — Discuss Design Before Implementation

**Before writing any code for a new feature, you MUST stop and discuss the design with the user first.** Present:

1. The proposed data structures and type definitions.
2. The proposed function / module interfaces.
3. Key trade-offs and alternatives considered.
4. Open questions or assumptions.

**Do not start implementation until the user has confirmed the design is sound and efficient.** Spend ten minutes thinking and talking before you code — it prevents hours of rework.

This rule applies to every new feature and every non-trivial change. Small, localized edits — typo fixes, one-line bug fixes, mechanical refactors — are exempt.
