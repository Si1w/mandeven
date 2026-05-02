//! Built-in skills shipped with mandeven.
//!
//! Built-ins are seeded into `~/.mandeven/skills/` instead of being
//! invoked directly from compiled strings. That keeps the runtime
//! model simple — every skill is a normal editable `SKILL.md`.

use std::fs;
use std::path::Path;

use super::error::{Error, Result};
use super::{SKILL_FILENAME, SKILLS_SUBDIR};

struct BuiltinSkill {
    name: &'static str,
    content: &'static str,
}

const BUILTINS: &[BuiltinSkill] = &[
    BuiltinSkill {
        name: "cron",
        content: include_str!("builtin/cron/SKILL.md"),
    },
    BuiltinSkill {
        name: "heartbeat",
        content: include_str!("builtin/heartbeat/SKILL.md"),
    },
    BuiltinSkill {
        name: "memorize",
        content: include_str!("builtin/memorize/SKILL.md"),
    },
];

/// Seed built-in skills into `<data_dir>/skills/` when missing.
///
/// Existing files are never overwritten. Editing
/// `~/.mandeven/skills/<name>/SKILL.md` is therefore the normal way
/// to customize a built-in skill.
///
/// # Errors
///
/// Returns [`Error::BuiltinSeed`] when a built-in skill directory or
/// `SKILL.md` file cannot be created.
pub fn seed(data_dir: &Path) -> Result<()> {
    let root = data_dir.join(SKILLS_SUBDIR);
    for builtin in BUILTINS {
        let destination = root.join(builtin.name);
        let skill_path = destination.join(SKILL_FILENAME);
        if skill_path.exists() {
            continue;
        }
        fs::create_dir_all(&destination).map_err(|source| Error::BuiltinSeed {
            path: destination.clone(),
            source,
        })?;
        fs::write(&skill_path, builtin.content).map_err(|source| Error::BuiltinSeed {
            path: skill_path,
            source,
        })?;
    }
    Ok(())
}
