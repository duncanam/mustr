//! Editable Markdown roles with optional TOML front matter and single inheritance.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use color_eyre::eyre::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::paths::check_regular;

pub(crate) const DEFAULTS: &[(&str, &str)] = &[
    ("architect", include_str!("../roles/architect.md")),
    ("implementor", include_str!("../roles/implementor.md")),
    ("reviewer", include_str!("../roles/reviewer.md")),
    ("researcher", include_str!("../roles/researcher.md")),
];

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Header {
    description: String,
    extends: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Role {
    pub name: String,
    pub description: String,
    pub prompt: String,
}

pub(crate) fn valid_name(name: &str) -> bool {
    (1..=48).contains(&name.len())
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub(crate) fn load(directory: &Path) -> Result<Vec<Role>> {
    let mut raw = BTreeMap::new();
    for entry in fs::read_dir(directory)
        .wrap_err_with(|| format!("reading roles in {}", directory.display()))?
    {
        let path = entry?.path();
        if path.extension().is_none_or(|s| s != "md") {
            continue;
        }
        check_regular(&path)?;
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        ensure!(
            valid_name(&name),
            "invalid role filename {} (use lowercase letters, digits, hyphens)",
            path.display()
        );
        let text = fs::read_to_string(&path)?;
        ensure!(
            text.len() <= 64 * 1024,
            "role {} exceeds 64 KiB",
            path.display()
        );
        let (header, prompt) =
            parse(&text).wrap_err_with(|| format!("parsing {}", path.display()))?;
        raw.insert(name, (header, prompt));
    }
    let mut resolved = BTreeMap::new();
    for name in raw.keys() {
        resolve(name, &raw, &mut resolved, &mut BTreeSet::new())?;
    }
    ensure!(
        !resolved.is_empty(),
        "no roles in {}; create a Markdown role",
        directory.display()
    );
    Ok(resolved.into_values().collect())
}

fn parse(text: &str) -> Result<(Header, String)> {
    let normalized = text.replace("\r\n", "\n");
    let (header, body) = if let Some(rest) = normalized.strip_prefix("+++\n") {
        let (header, body) = rest.split_once("\n+++\n").ok_or_else(|| {
            color_eyre::eyre::eyre!("unclosed TOML front matter (expected +++ on a line)")
        })?;
        (toml::from_str(header)?, body)
    } else {
        (Header::default(), normalized.as_str())
    };
    ensure!(!body.trim().is_empty(), "role prompt must not be empty");
    Ok((header, body.trim().to_owned()))
}

fn resolve(
    name: &str,
    raw: &BTreeMap<String, (Header, String)>,
    resolved: &mut BTreeMap<String, Role>,
    visiting: &mut BTreeSet<String>,
) -> Result<Role> {
    if let Some(role) = resolved.get(name) {
        return Ok(role.clone());
    }
    ensure!(
        visiting.insert(name.to_owned()),
        "role inheritance cycle at {name}"
    );
    let Some((header, body)) = raw.get(name) else {
        bail!("unknown parent role {name}");
    };
    let prompt = match &header.extends {
        Some(parent) => format!(
            "{}\n\n{}",
            resolve(parent, raw, resolved, visiting)?.prompt,
            body
        ),
        None => body.clone(),
    };
    ensure!(
        prompt.len() <= 128 * 1024,
        "resolved role {name} exceeds 128 KiB"
    );
    visiting.remove(name);
    let role = Role {
        name: name.to_owned(),
        description: header.description.clone(),
        prompt,
    };
    resolved.insert(name.to_owned(), role.clone());
    Ok(role)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_role_extends_an_editable_parent() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("base.md"), "Keep scope narrow.").unwrap();
        fs::write(
            dir.path().join("sql-reviewer.md"),
            "+++\nextends = \"base\"\ndescription = \"SQL\"\n+++\nReview SQL.",
        )
        .unwrap();
        let roles = load(dir.path()).unwrap();
        assert_eq!(roles[1].prompt, "Keep scope narrow.\n\nReview SQL.");
    }

    #[test]
    fn invalid_inheritance_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "+++\nextends = \"b\"\n+++\nA").unwrap();
        assert!(load(dir.path()).is_err());
        fs::write(dir.path().join("b.md"), "+++\nextends = \"a\"\n+++\nB").unwrap();
        assert!(load(dir.path()).unwrap_err().to_string().contains("cycle"));
    }

    #[test]
    fn rejects_unknown_header_keys_and_empty_prompts() {
        assert!(parse("+++\nextend = \"base\"\n+++\nx").is_err());
        assert!(parse("\n").is_err());
        assert!(parse("+++\n").is_err());
    }
}
