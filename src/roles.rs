//! Editable Markdown roles with optional TOML front matter and ordered composition.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use color_eyre::eyre::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::paths::check_regular;

const MAX_ROLE_BYTES: usize = 64 * 1024;
const MAX_EXPANDED_BYTES: usize = 128 * 1024;
const MAX_ROLE_NESTING_DEPTH: usize = 32;
const MAX_ROLE_GRAPH_SIZE: usize = 128;

pub(crate) const DEFAULTS: &[(&str, &str)] = &[
    ("architect", include_str!("../roles/architect.md")),
    ("base", include_str!("../roles/base.md")),
    ("implementor", include_str!("../roles/implementor.md")),
    ("reviewer", include_str!("../roles/reviewer.md")),
    ("researcher", include_str!("../roles/researcher.md")),
];

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Header {
    description: String,
    compose: Vec<String>,
    manager: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Role {
    pub name: String,
    pub description: String,
    pub prompt: String,
    #[serde(default)]
    pub manager: bool,
}

pub(crate) fn valid_name(name: &str) -> bool {
    (1..=48).contains(&name.len())
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Resolve exactly one requested graph; unrelated invalid roles are never read.
pub(crate) fn resolve(directory: &Path, name: &str) -> Result<Role> {
    ensure!(
        valid_name(name),
        "role dependency path {name}: invalid role name"
    );
    let mut resolution = Resolution::new(directory);
    resolution.visit(name)?;
    Ok(Role {
        name: name.to_owned(),
        description: resolution
            .description
            .expect("successfully visited root role"),
        prompt: resolution.expanded,
        manager: resolution.manager,
    })
}

/// List each valid role independently, warning and continuing past invalid graphs.
pub(crate) fn load(directory: &Path) -> Result<Vec<Role>> {
    let entries = fs::read_dir(directory)
        .wrap_err_with(|| format!("reading roles in {}", directory.display()))?;
    let mut names = BTreeSet::new();
    let mut warnings = Vec::new();
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                warnings.push(format!("could not inspect a role directory entry: {error}"));
                continue;
            }
        };
        if path.extension().is_none_or(|extension| extension != "md") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_owned();
        if !valid_name(&name) {
            warnings.push(format!(
                "invalid role filename {} (use lowercase letters, digits, hyphens)",
                path.display()
            ));
        } else {
            names.insert(name);
        }
    }

    let mut roles = Vec::new();
    for name in names {
        match resolve(directory, &name) {
            Ok(role) => roles.push(role),
            Err(error) => warnings.push(format!("invalid role {name}: {error:#}")),
        }
    }
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    ensure!(
        !roles.is_empty(),
        "no valid roles in {}; create a Markdown role",
        directory.display()
    );
    Ok(roles)
}

struct Resolution<'a> {
    directory: &'a Path,
    description: Option<String>,
    manager: bool,
    active: Vec<String>,
    discovered: BTreeSet<String>,
    visited: BTreeSet<String>,
    expanded: String,
}

impl<'a> Resolution<'a> {
    fn new(directory: &'a Path) -> Self {
        Self {
            directory,
            description: None,
            manager: false,
            active: Vec::new(),
            discovered: BTreeSet::new(),
            visited: BTreeSet::new(),
            expanded: String::new(),
        }
    }

    fn path(&self, next: &str) -> String {
        self.active
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(next))
            .collect::<Vec<_>>()
            .join(" -> ")
    }

    fn visit(&mut self, name: &str) -> Result<()> {
        let dependency_path = self.path(name);
        ensure!(
            valid_name(name),
            "role dependency path {dependency_path}: invalid component name"
        );
        if let Some(index) = self.active.iter().position(|active| active == name) {
            let mut cycle = self.active[index..].to_vec();
            cycle.push(name.to_owned());
            bail!(
                "role dependency path {dependency_path}: composition cycle ({})",
                cycle.join(" -> ")
            );
        }
        if self.visited.contains(name) {
            return Ok(());
        }
        ensure!(
            self.active.len() < MAX_ROLE_NESTING_DEPTH,
            "role dependency path {dependency_path} exceeds maximum composition nesting depth of {MAX_ROLE_NESTING_DEPTH}"
        );
        if self.discovered.insert(name.to_owned()) {
            ensure!(
                self.discovered.len() <= MAX_ROLE_GRAPH_SIZE,
                "role dependency path {dependency_path} exceeds maximum graph size of {MAX_ROLE_GRAPH_SIZE}"
            );
        }

        self.active.push(name.to_owned());
        let result = self.visit_active(name);
        self.active.pop();
        result
    }

    fn visit_active(&mut self, name: &str) -> Result<()> {
        let dependency_path = self.active.join(" -> ");
        let role_path = self.directory.join(format!("{name}.md"));
        let metadata = match fs::symlink_metadata(&role_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                bail!("role dependency path {dependency_path}: unknown role {name}")
            }
            Err(error) => {
                return Err(error).wrap_err_with(|| {
                    format!(
                        "role dependency path {dependency_path}: reading {}",
                        role_path.display()
                    )
                });
            }
        };
        check_regular(&role_path).wrap_err_with(|| {
            format!(
                "role dependency path {dependency_path}: loading {}",
                role_path.display()
            )
        })?;
        ensure!(
            metadata.len() <= MAX_ROLE_BYTES as u64,
            "role dependency path {dependency_path}: role {name} exceeds 64 KiB"
        );
        let text = fs::read_to_string(&role_path).wrap_err_with(|| {
            format!(
                "role dependency path {dependency_path}: reading {}",
                role_path.display()
            )
        })?;
        ensure!(
            text.len() <= MAX_ROLE_BYTES,
            "role dependency path {dependency_path}: role {name} exceeds 64 KiB"
        );
        let (header, prompt) = parse(&text).map_err(|error| {
            color_eyre::eyre::eyre!(
                "role dependency path {dependency_path}: parsing {}: {error:#}",
                role_path.display()
            )
        })?;
        if self.active.len() == 1 {
            self.description = Some(header.description.clone());
            // Manager is admission metadata from the selected role only. A
            // composed component may contribute prompt text, never authority.
            self.manager = header.manager;
        }

        let mut direct = BTreeMap::<&str, usize>::new();
        for (index, component) in header.compose.iter().enumerate() {
            let component_path = self.path(component);
            ensure!(
                valid_name(component),
                "role dependency path {component_path}: invalid component name"
            );
            if let Some(first_index) = direct.insert(component, index) {
                bail!(
                    "role {name} references component {component} more than once; duplicate dependency paths: {dependency_path} -> {component} (compose[{first_index}]) and {dependency_path} -> {component} (compose[{index}])"
                );
            }
            self.visit(component)?;
        }

        let separator = if self.expanded.is_empty() { "" } else { "\n\n" };
        let expanded_size = self
            .expanded
            .len()
            .checked_add(separator.len())
            .and_then(|size| size.checked_add(prompt.len()))
            .unwrap_or(usize::MAX);
        ensure!(
            expanded_size <= MAX_EXPANDED_BYTES,
            "role dependency path {dependency_path}: expanded prompt exceeds 128 KiB"
        );
        self.expanded.push_str(separator);
        self.expanded.push_str(&prompt);
        self.visited.insert(name.to_owned());
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_role(directory: &Path, name: &str, compose: &[&str], body: &str) {
        let front_matter = if compose.is_empty() {
            String::new()
        } else {
            format!(
                "+++\ncompose = [{}]\n+++\n",
                compose
                    .iter()
                    .map(|name| format!("\"{name}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        fs::write(
            directory.join(format!("{name}.md")),
            format!("{front_matter}{body}"),
        )
        .unwrap();
    }

    #[test]
    fn composes_nested_roles_in_listed_depth_first_order_and_deduplicates_diamonds() {
        let dir = tempfile::tempdir().unwrap();
        write_role(dir.path(), "shared", &[], "Shared.");
        write_role(dir.path(), "first", &["shared"], "First.");
        write_role(dir.path(), "second", &["shared"], "Second.");
        fs::write(
            dir.path().join("root.md"),
            "+++\ndescription = \"root role\"\ncompose = [\"first\", \"second\"]\n+++\nRoot.",
        )
        .unwrap();

        let role = resolve(dir.path(), "root").unwrap();
        assert_eq!(role.name, "root");
        assert_eq!(role.description, "root role");
        assert_eq!(role.prompt, "Shared.\n\nFirst.\n\nSecond.\n\nRoot.");
    }

    #[test]
    fn rejects_cycles_unknown_components_invalid_names_and_legacy_extends_with_paths() {
        let dir = tempfile::tempdir().unwrap();
        write_role(dir.path(), "a", &["b"], "A.");
        write_role(dir.path(), "b", &["a"], "B.");
        let error = resolve(dir.path(), "a").unwrap_err().to_string();
        assert!(error.contains("cycle"));
        assert!(error.contains("a -> b -> a"));
        write_role(dir.path(), "self-cycle", &["self-cycle"], "Self.");
        let error = resolve(dir.path(), "self-cycle").unwrap_err().to_string();
        assert!(error.contains("cycle"));
        assert!(error.contains("self-cycle -> self-cycle"));

        write_role(dir.path(), "missing-parent", &["absent"], "Parent.");
        let error = resolve(dir.path(), "missing-parent")
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing-parent -> absent"));
        assert!(error.contains("unknown role absent"));

        write_role(dir.path(), "invalid-child", &["Bad_name"], "Parent.");
        let error = resolve(dir.path(), "invalid-child")
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid-child -> Bad_name"));
        assert!(error.contains("invalid component name"));

        fs::write(
            dir.path().join("legacy.md"),
            "+++\nextends = \"a\"\n+++\nLegacy.",
        )
        .unwrap();
        let error = resolve(dir.path(), "legacy").unwrap_err().to_string();
        assert!(error.contains("extends"));
        fs::write(
            dir.path().join("mixed.md"),
            "+++\ncompose = [\"a\"]\nextends = \"a\"\n+++\nMixed.",
        )
        .unwrap();
        let error = resolve(dir.path(), "mixed").unwrap_err().to_string();
        assert!(error.contains("extends"));
    }

    #[test]
    fn malformed_component_and_invalid_requested_names_report_paths() {
        let dir = tempfile::tempdir().unwrap();
        write_role(dir.path(), "root", &["component"], "Root.");
        fs::write(
            dir.path().join("component.md"),
            "+++\ncompose = [\n+++\nMalformed component.",
        )
        .unwrap();
        let error = resolve(dir.path(), "root").unwrap_err().to_string();
        assert!(error.contains("root -> component"));
        assert!(error.contains("parsing"));

        let error = resolve(dir.path(), "../outside").unwrap_err().to_string();
        assert!(error.contains("role dependency path ../outside"));
        assert!(error.contains("invalid role name"));
    }

    #[test]
    fn rejects_duplicate_direct_entries_with_both_dependency_paths() {
        let dir = tempfile::tempdir().unwrap();
        write_role(dir.path(), "component", &[], "Component.");
        write_role(
            dir.path(),
            "duplicate",
            &["component", "component"],
            "Root.",
        );
        let error = resolve(dir.path(), "duplicate").unwrap_err().to_string();
        assert!(error.contains("duplicate dependency paths"));
        assert!(error.contains("duplicate -> component (compose[0])"));
        assert!(error.contains("duplicate -> component (compose[1])"));
    }

    #[test]
    fn applies_depth_and_graph_size_limits_per_requested_resolution() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..MAX_ROLE_NESTING_DEPTH {
            let name = format!("depth-{index}");
            let next = (index + 1 < MAX_ROLE_NESTING_DEPTH).then(|| format!("depth-{}", index + 1));
            let compose = next.as_deref().into_iter().collect::<Vec<_>>();
            write_role(dir.path(), &name, &compose, "Depth.");
        }
        assert!(resolve(dir.path(), "depth-0").is_ok());
        write_role(dir.path(), "depth-31", &["depth-32"], "Depth.");
        write_role(dir.path(), "depth-32", &[], "Too deep.");
        let error = resolve(dir.path(), "depth-0").unwrap_err().to_string();
        assert!(error.contains("maximum composition nesting depth of 32"));
        assert!(error.contains("depth-0 -> depth-1"));

        write_role(dir.path(), "shared-depth", &[], "Shared at shallow depth.");
        for index in 0..MAX_ROLE_NESTING_DEPTH - 1 {
            let name = format!("chain-{index}");
            let compose = if index + 1 < MAX_ROLE_NESTING_DEPTH - 1 {
                vec![format!("chain-{}", index + 1)]
            } else {
                vec!["shared-depth".to_owned()]
            };
            write_role(
                dir.path(),
                &name,
                &compose.iter().map(String::as_str).collect::<Vec<_>>(),
                "Chain.",
            );
        }
        write_role(
            dir.path(),
            "deep-root",
            &["shared-depth", "chain-0"],
            "Root.",
        );
        assert!(resolve(dir.path(), "deep-root").is_ok());
        write_role(dir.path(), "new-depth-child", &[], "New child.");
        write_role(
            dir.path(),
            &format!("chain-{}", MAX_ROLE_NESTING_DEPTH - 2),
            &["shared-depth", "new-depth-child"],
            "Chain end.",
        );
        let error = resolve(dir.path(), "deep-root").unwrap_err().to_string();
        assert!(error.contains("maximum composition nesting depth of 32"));
        assert!(error.contains("deep-root -> chain-0"));
        assert!(error.contains("new-depth-child"));

        let components = (0..MAX_ROLE_GRAPH_SIZE)
            .map(|index| format!("node-{index:03}"))
            .collect::<Vec<_>>();
        for name in &components {
            write_role(dir.path(), name, &[], "Node.");
        }
        let references = components.iter().map(String::as_str).collect::<Vec<_>>();
        write_role(
            dir.path(),
            "graph-ok",
            &references[..MAX_ROLE_GRAPH_SIZE - 1],
            "Root.",
        );
        assert!(resolve(dir.path(), "graph-ok").is_ok());
        write_role(dir.path(), "graph-too-large", &references, "Root.");
        let error = resolve(dir.path(), "graph-too-large")
            .unwrap_err()
            .to_string();
        assert!(error.contains("maximum graph size of 128"));
    }

    #[test]
    fn applies_individual_and_expanded_prompt_size_limits() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("large.md"), "x".repeat(MAX_ROLE_BYTES + 1)).unwrap();
        let error = resolve(dir.path(), "large").unwrap_err().to_string();
        assert!(error.contains("exceeds 64 KiB"));

        for name in ["chunk-a", "chunk-b", "chunk-c"] {
            fs::write(dir.path().join(format!("{name}.md")), "x".repeat(50 * 1024)).unwrap();
        }
        write_role(
            dir.path(),
            "expanded",
            &["chunk-a", "chunk-b", "chunk-c"],
            "End.",
        );
        let error = resolve(dir.path(), "expanded").unwrap_err().to_string();
        assert!(error.contains("expanded prompt exceeds 128 KiB"));
    }

    #[test]
    fn lists_valid_roles_without_a_bad_unrelated_graph_blocking_them() {
        let dir = tempfile::tempdir().unwrap();
        write_role(dir.path(), "valid", &[], "Valid.");
        write_role(dir.path(), "invalid", &["missing"], "Invalid.");
        let roles = load(dir.path()).unwrap();
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].name, "valid");
    }

    #[test]
    fn resolver_concatenates_role_text_but_preserves_the_conflict_contract() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("roles");
        let dir = tempfile::tempdir().unwrap();
        for name in ["base", "architect", "implementor", "reviewer", "researcher"] {
            fs::copy(
                source.join(format!("{name}.md")),
                dir.path().join(format!("{name}.md")),
            )
            .unwrap();
        }
        write_role(
            dir.path(),
            "combined",
            &["architect", "implementor", "reviewer", "researcher"],
            "Combined role.",
        );

        let role = resolve(dir.path(), "combined").unwrap();
        let (_, base_prompt) =
            parse(&fs::read_to_string(dir.path().join("base.md")).unwrap()).unwrap();
        assert_eq!(role.prompt.matches(&base_prompt).count(), 1);
        assert!(role.prompt.contains(
            "report the conflict and obtain the human's direct selection of one functional role before proceeding"
        ));
        let lower = role.prompt.to_ascii_lowercase();
        for override_text in [
            "ignore upstream instructions",
            "ignore implementor instructions",
            "ignore instructions inherited from implementor",
        ] {
            assert!(
                !lower.contains(override_text),
                "unexpected override: {override_text}"
            );
        }
        for name in ["architect", "implementor", "reviewer", "researcher"] {
            let (_, prompt) =
                parse(&fs::read_to_string(dir.path().join(format!("{name}.md"))).unwrap()).unwrap();
            assert!(
                role.prompt.contains(&prompt),
                "missing {name} responsibility"
            );
        }
    }

    #[test]
    fn manager_metadata_is_defaulted_root_only_and_type_checked() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("component.md"),
            "+++\nmanager = true\n+++\nComponent.",
        )
        .unwrap();
        fs::write(
            dir.path().join("ordinary-root.md"),
            "+++\ncompose = [\"component\"]\n+++\nRoot.",
        )
        .unwrap();
        fs::write(
            dir.path().join("manager-root.md"),
            "+++\nmanager = true\ncompose = [\"component\"]\n+++\nRoot.",
        )
        .unwrap();
        fs::write(
            dir.path().join("invalid-manager.md"),
            "+++\nmanager = \"yes\"\n+++\nRoot.",
        )
        .unwrap();

        assert!(resolve(dir.path(), "component").unwrap().manager);
        assert!(!resolve(dir.path(), "ordinary-root").unwrap().manager);
        assert!(resolve(dir.path(), "manager-root").unwrap().manager);
        assert!(resolve(dir.path(), "invalid-manager").is_err());
    }

    #[test]
    fn rejects_unknown_header_keys_and_empty_prompts() {
        assert!(parse("+++\nextend = \"base\"\n+++\nx").is_err());
        assert!(parse("+++\nextends = \"base\"\n+++\nx").is_err());
        assert!(parse("\n").is_err());
        assert!(parse("+++\n").is_err());
    }
}
