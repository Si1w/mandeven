//! Utilities — cross-cutting helpers that don't belong to any single
//! domain module.
//!
//! Each submodule owns one well-scoped concern. Keep this module as a
//! thin namespace: if a helper grows enough to warrant its own
//! domain (e.g. its own error types, config block, or non-trivial
//! state), promote it to a top-level crate module instead of letting
//! `utils` swell into a catchall.

pub mod workspace;
