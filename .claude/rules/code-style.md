# Coding Standards

Chisel follows Section 2 (Code Style) of the Rust Coding Guidelines (Chinese community edition).

- **G.\*** rules are **required** — key content inlined below.
- **P.\*** rules are **recommended** — listed by title only.

Section 3 (Coding Practice) is **not** adopted wholesale. Pull in individual rules from Section 3 only when the task at hand touches the relevant area (e.g. consult the `unsafe` rules before writing FFI).

---

## 2.1 Naming

### Required

#### G.NAM.01 — Use a unified naming convention

| Item | Convention |
| --- | --- |
| Crates | `snake_case` |
| Modules | `snake_case` |
| Types, Traits, Enum variants | `UpperCamelCase` |
| Functions, Methods, Local variables | `snake_case` |
| Statics, Constants | `SCREAMING_SNAKE_CASE` |
| Type parameters | `UpperCamelCase` (usually single-letter, e.g. `T`) |
| Lifetimes | short lowercase, e.g. `'a` |
| Macros | `snake_case!` |

Acronyms inside `UpperCamelCase` count as a single word: write `Uuid`, not `UUID`.

#### G.NAM.02 — Conversion function names must match ownership semantics

| Prefix | Cost | Ownership | When to use |
| --- | --- | --- | --- |
| `as_` | Free | borrowed → borrowed | Cheap view into existing data, no allocation (e.g. `str::as_bytes`) |
| `to_` | Expensive | borrowed → owned / borrowed | Same abstraction level but does real work, e.g. allocates (e.g. `str::to_lowercase`) |
| `into_` | Varies | owned → owned | Consumes `self` and decomposes the data (e.g. `String::into_bytes`) |

When the method returns a mutable reference, `mut` goes directly after the type: `as_mut_slice`, not `as_slice_mut`.

### Recommended

- **P.NAM.01** — Use a consistent word order for identifiers across a crate.
- **P.NAM.02** — Don't use filler words in cargo feature names.
- **P.NAM.03** — Identifier names should read naturally.
- **P.NAM.04** — The larger the scope, the more precise the name; the smaller the scope, the shorter it can be.
- **P.NAM.05** — Don't prefix getters with `get_`.
- **P.NAM.06** — Follow the `iter` / `iter_mut` / `into_iter` convention when producing iterators.
- **P.NAM.07** — Avoid reserved words, keywords, built-in types, and built-in trait names.
- **P.NAM.08** — Don't encode the type in variable names.
- **P.NAM.09** — Prefix global statics with `G_` to distinguish them from constants.

---

## 2.2 Formatting

Every rule in this section is P-level. Running `rustfmt` (P.FMT.01) enforces most of them automatically, so in practice the rule is: **run `cargo fmt` before every commit.**

- **P.FMT.01** — Format all code with `rustfmt`.
- **P.FMT.02** — Indent with spaces, not tabs.
- **P.FMT.03** — At most one blank line between items.
- **P.FMT.04** — Opening brace on the same line as the item definition.
- **P.FMT.05** — Preserve block indentation when multiple identifiers are present.
- **P.FMT.06** — In multi-line expressions, put operators at the start of the line.
- **P.FMT.07** — Enum variants and struct fields are left-aligned.
- **P.FMT.08** — Wrap when a function has more than 5 params or an import pulls more than 4 modules.
- **P.FMT.09** — Use context-appropriate spacing.
- **P.FMT.10** — Keep `match` arms readable.
- **P.FMT.11** — Group imports readably.
- **P.FMT.12** — Keep declarative macro arms readable.
- **P.FMT.13** — Don't omit field names when initializing named-field structs.
- **P.FMT.14** — `extern` functions must explicitly specify the C ABI.
- **P.FMT.15** — `..` is fine for ignoring remaining tuple elements.
- **P.FMT.16** — Don't merge unrelated derive traits onto one `#[derive(...)]` line.

---

## 2.3 Comments

### Required

#### G.CMT.01 — Public `fn -> Result<...>` must document an `# Errors` section

```rust
/// # Errors
///
/// Will return `Err` if `filename` does not exist or the user lacks read permission.
pub fn read(filename: String) -> io::Result<String> {
    unimplemented!();
}
```

Enforced by clippy's `missing_errors_doc` lint.

#### G.CMT.02 — Public APIs that can panic must document a `# Panics` section

```rust
/// # Panics
///
/// Will panic if `y` is 0.
pub fn divide_by(x: i32, y: i32) -> i32 {
    if y == 0 {
        panic!("Cannot divide by 0")
    } else {
        x / y
    }
}
```

Enforced by `#![warn(clippy::missing_panics_doc)]`.

#### G.CMT.03 — Use 4 spaces (not tabs) inside doc comments

Applies to lists, code blocks, and nested structures inside `///`. Enforced by clippy's `tabs_in_doc_comments` lint.

### Recommended

- **P.CMT.01** — Let code self-document; keep doc comments crisp.
- **P.CMT.02** — Doc comments should have a width limit.
- **P.CMT.03** — Prefer line comments (`//`) over block comments (`/* */`).
- **P.CMT.04** — File header comment should include a copyright notice.
- **P.CMT.05** — Use `FIXME` and `TODO` in comments for task coordination.

---

## Enforcement

Minimal checks that cover the required rules above:

```bash
cargo fmt --check
cargo clippy -- \
    -W clippy::missing_errors_doc \
    -W clippy::missing_panics_doc \
    -W clippy::tabs_in_doc_comments
```
