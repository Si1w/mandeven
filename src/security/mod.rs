//! Cross-cutting security primitives: the active sandbox tier and the
//! command allow-list that gates [`crate::tools::shell`] under read-only.
//!
//! Lives outside [`crate::tools`] on purpose. Tools are *enforcement
//! sites*; this module is the *policy*. Splitting them keeps tools free
//! of policy decisions (each tool just asks `ensure_*` whether it may
//! proceed) and gives a single place to grow toward Milestone B's
//! approval workflow without scattering policy across every tool file.
//!
//! Re-exports the most commonly used items so callers can write
//! `use mandeven::security::SandboxPolicy` rather than reaching through
//! the inner submodules.

pub mod commands;
pub mod network;
pub mod policy;

pub use commands::ensure_safe_command;
pub use network::{NetworkError, validate_resolved_host, validate_url_target};
pub use policy::{SandboxConfig, SandboxPolicy, ensure_writable_now};
