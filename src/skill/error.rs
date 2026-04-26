//! Errors surfaced by the `skill` module.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Failures from skill discovery, parsing, or invocation.
#[derive(Debug, Error)]
pub enum Error {
    /// The skills directory exists but a read against it failed.
    /// Distinguished from "missing directory": absent dir is normal
    /// ([`crate::skill::SkillIndex::load`] returns an empty index in
    /// that case).
    #[error("failed to read skills directory {}: {source}", path.display())]
    DirRead {
        /// Resolved on-disk path.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// `SKILL.md` exists but the read failed.
    #[error("failed to read SKILL.md at {}: {source}", path.display())]
    SkillRead {
        /// Resolved on-disk path.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// The frontmatter block is missing or malformed (no opening
    /// `---`, no closing `---`, or unparseable key/value lines).
    #[error("malformed frontmatter in {}: {reason}", path.display())]
    FrontmatterParse {
        /// Resolved on-disk path.
        path: PathBuf,
        /// Human-readable explanation.
        reason: String,
    },

    /// A required frontmatter field is absent.
    #[error("missing required field {field:?} in frontmatter of {}", path.display())]
    MissingField {
        /// Resolved on-disk path.
        path: PathBuf,
        /// Field that was expected.
        field: &'static str,
    },

    /// Frontmatter `name` does not match the on-disk directory.
    /// Mismatched names would let `/foo` and `/bar` resolve to the
    /// same SKILL.md, breaking 1:1 invocation routing.
    #[error("skill name {declared:?} in {} does not match directory {dir:?}", path.display())]
    NameMismatch {
        /// Resolved on-disk path of the SKILL.md.
        path: PathBuf,
        /// Name declared in frontmatter.
        declared: String,
        /// Directory name (also the slash-command name).
        dir: String,
    },
}

/// Result alias for the `skill` module.
pub type Result<T> = std::result::Result<T, Error>;
