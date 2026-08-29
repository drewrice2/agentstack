use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{SKILL_MD, ValidationOutcome, validate_skill};

/// A direct child directory containing a `SKILL.md`, plus its validation
/// result. Discovery is intentionally shallow to match `agentstack skill scan`.
#[derive(Debug)]
pub struct DiscoveredSkill {
    pub name: String,
    pub path: PathBuf,
    pub validation: ValidationOutcome,
}

pub fn discover_skills(root: &Path) -> Result<Vec<DiscoveredSkill>> {
    let mut found = Vec::new();

    for entry in
        fs::read_dir(root).with_context(|| format!("failed to read `{}`", root.display()))?
    {
        let entry = entry.context("failed to read directory entry")?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if !path.join(SKILL_MD).is_file() {
            continue;
        }

        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let name = name.to_string();
        let validation = validate_skill(&path);
        found.push(DiscoveredSkill {
            name,
            path,
            validation,
        });
    }

    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}
