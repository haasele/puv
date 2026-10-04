use std::collections::{BTreeMap, BTreeSet, HashMap};

use puv_core::{LockFile, LockedPackage, Manifest};
use puv_registry::{Advisory, HttpRegistry, MetadataProvider};
use puv_semver::{Constraint, Version};

fn tone(text: &str, code: &str) -> String {
    puv_core::paint(text, code, puv_core::stdout_is_tty())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Prod,
    Dev,
    Tool,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Prod => "",
            Self::Dev => " dev",
            Self::Tool => " tool",
        }
    }
}

struct RootEdge {
    name: String,
    constraint: String,
    kind: Kind,
}

pub fn render_list(manifest: &Manifest, lock: Option<&LockFile>) -> String {
    let mut out = String::new();
    out.push_str(&manifest.project.name);
    out.push('\n');
    let php = lock
        .map(|lock| lock.runtime.php.as_str())
        .unwrap_or(manifest.project.php.as_str());
    let extensions = lock
        .map(|lock| lock.runtime.extensions.clone())
        .unwrap_or_default();
    let packages = lock.map(|lock| index_packages(&lock.packages));
    let roots = root_edges(manifest);
    let mut lines = Vec::new();
    lines.push(Node {
        label: format!("php@{php}"),
        note: String::new(),
        children: extensions
            .iter()
            .map(|name| Node {
                label: format!("ext-{name}"),
                note: String::new(),
                children: Vec::new(),
            })
            .collect(),
    });
    if let Some(packages) = packages.as_ref() {
        let mut seen = BTreeSet::new();
        for edge in &roots {
            if edge.kind == Kind::Tool {
                continue;
            }
            if let Some(node) = package_node(packages, edge, &mut seen) {
                lines.push(node);
            }
        }
    }
    push_tree(&mut out, "", &lines);
    if let Some(lock) = lock
        && !lock.tools.is_empty()
    {
        out.push_str("\ntools\n");
        let mut tool_lines = Vec::new();
        for tool in &lock.tools {
            let packages = index_packages(&tool.packages);
            let edge = RootEdge {
                name: tool.name.clone(),
                constraint: String::new(),
                kind: Kind::Tool,
            };
            let mut seen = BTreeSet::new();
            if let Some(node) = package_node(&packages, &edge, &mut seen) {
                tool_lines.push(node);
            } else {
                tool_lines.push(Node {
                    label: format!("{}@{}", tool.name, tool.version),
                    note: String::new(),
                    children: Vec::new(),
                });
            }
        }
        push_tree(&mut out, "", &tool_lines);
    }
    if packages.as_ref().is_none_or(|packages| packages.is_empty()) && extensions.is_empty() {
        out.push_str("└─ (no packages)\n");
    }
    out
}

pub fn render_why(manifest: &Manifest, lock: &LockFile, query: &str) -> Result<String, String> {
    let name = puv_core::normalize_name(query);
    if let Some(extension) = name.strip_prefix("ext-")
        && lock.runtime.extensions.iter().any(|item| item == extension)
    {
        return Ok(format!(
            "{}\n{} php@{} provides it\n",
            tone(&name, "1;36"),
            tone("└─", "2"),
            tone(&lock.runtime.php, "32")
        ));
    }
    let packages = index_packages(&lock.packages);
    let mut paths = Vec::new();
    for edge in root_edges(manifest) {
        if edge.kind == Kind::Tool {
            continue;
        }
        let mut stack = Vec::new();
        walk_why(&packages, &edge, &name, &mut stack, &mut paths);
    }
    for tool in &lock.tools {
        let tool_packages = index_packages(&tool.packages);
        let edge = RootEdge {
            name: tool.name.clone(),
            constraint: manifest
                .tool_dependencies
                .get(&tool.name)
                .cloned()
                .unwrap_or_default(),
            kind: Kind::Tool,
        };
        let mut stack = Vec::new();
        walk_why(&tool_packages, &edge, &name, &mut stack, &mut paths);
    }
    if paths.is_empty() {
        return Err(format!("{name} is not installed"));
    }
    let version = paths
        .iter()
        .find_map(|path| path.last().map(|step| step.version.clone()))
        .unwrap_or_default();
    let project = &manifest.project.name;
    let mut out = tone(&name, "1;36");
    if !version.is_empty() {
        out.push('@');
        out.push_str(&tone(&version, "32"));
    }
    out.push('\n');
    for path in &paths {
        let direct = &path[0];
        let via: Vec<_> = path.iter().rev().skip(1).collect();
        for (index, step) in via.iter().enumerate() {
            let pad = "   ".repeat(index);
            out.push_str(&format!(
                "{pad}{} {}@{}\n",
                tone("└─", "2"),
                tone(&step.name, "36"),
                tone(&step.version, "32")
            ));
        }
        let pad = "   ".repeat(via.len());
        out.push_str(&format!(
            "{pad}{} {} depends on {} {}{}\n",
            tone("└─", "2"),
            tone(project, "36"),
            tone(&direct.name, "36"),
            tone(&direct.constraint, "33"),
            direct.kind.label()
        ));
    }
    Ok(out)
}

pub fn render_info(
    registry: &HttpRegistry,
    lock: Option<&LockFile>,
    query: &str,
) -> Result<String, String> {
    let (name, requested) = split_spec(query)?;
    let releases = registry.releases(&name).map_err(|err| err.to_string())?;
    if releases.is_empty() {
        return Err(format!("no metadata for {name}"));
    }
    let chosen = if let Some(requested) = &requested {
        releases
            .iter()
            .find(|release| release.version.to_string() == *requested)
            .ok_or_else(|| format!("{name} has no version {requested}"))?
    } else {
        releases
            .iter()
            .filter(|release| release.version.is_stable())
            .max_by(|left, right| left.version.cmp(&right.version))
            .or_else(|| {
                releases
                    .iter()
                    .max_by(|left, right| left.version.cmp(&right.version))
            })
            .ok_or_else(|| format!("no metadata for {name}"))?
    };
    let installed = lock.and_then(|lock| {
        lock.packages
            .iter()
            .chain(lock.tools.iter().flat_map(|tool| tool.packages.iter()))
            .find(|package| package.name == name)
            .map(|package| package.version.clone())
    });
    let latest = releases
        .iter()
        .filter(|release| release.version.is_stable())
        .max_by(|left, right| left.version.cmp(&right.version));
    let mut headline = vec![format!(
        "{}@{}",
        tone(&name, "1;36"),
        tone(&chosen.version.to_string(), "1;32")
    )];
    if !chosen.licenses.is_empty() {
        headline.push(tone(&chosen.licenses.join(" OR "), "33"));
    }
    if let Some(kind) = &chosen.package_type {
        headline.push(tone(kind, "2"));
    }
    headline.push(tone(&format!("deps: {}", chosen.dependencies.len()), "36"));
    headline.push(tone(&format!("versions: {}", releases.len()), "36"));
    let mut out = headline.join(&tone(" | ", "2"));
    out.push_str("\n\n");
    if let Some(description) = &chosen.description {
        out.push_str(description);
        out.push_str("\n\n");
    }
    if let Some(homepage) = &chosen.homepage {
        out.push_str(&tone(homepage, "36"));
        out.push_str("\n\n");
    }
    if !chosen.keywords.is_empty() {
        out.push_str(&format!(
            "{} {}\n\n",
            tone("keywords:", "2"),
            chosen.keywords.join(", ")
        ));
    }
    if let Some(installed) = installed {
        out.push_str(&format!(
            "{} {}\n\n",
            tone("installed:", "2"),
            tone(&installed, "32")
        ));
    }
    out.push_str(&format!(
        "{} ({}):\n",
        tone("dependencies", "1"),
        chosen.dependencies.len()
    ));
    if chosen.dependencies.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for (dependency, constraint) in &chosen.dependencies {
            out.push_str(&format!(
                "- {} {}\n",
                tone(dependency, "36"),
                tone(constraint, "33")
            ));
        }
    }
    if let Some(dist) = &chosen.dist {
        out.push_str(&format!("\n{}\n", tone("dist", "1")));
        out.push_str(&format!(" .tarball: {}\n", tone(&dist.url, "36")));
        if let Some(shasum) = &dist.shasum {
            out.push_str(&format!(" .shasum: {shasum}\n"));
        }
        out.push_str(&format!(" .type: {}\n", dist.kind));
    }
    if let Some(latest) = latest {
        out.push_str(&format!("\n{}\n", tone("dist-tags:", "1")));
        out.push_str(&format!(
            "latest: {}\n",
            tone(&latest.version.to_string(), "32")
        ));
    }
    if !chosen.authors.is_empty() {
        out.push_str(&format!("\n{}\n", tone("authors:", "1")));
        for author in &chosen.authors {
            out.push_str(&format!("- {}\n", tone(author, "36")));
        }
    }
    if let Some(published) = &chosen.published {
        out.push_str(&format!("\n{} {published}\n", tone("Published:", "2")));
    }
    Ok(out)
}

pub fn render_audit(
    registry: &HttpRegistry,
    manifest: &Manifest,
    lock: &LockFile,
) -> Result<(String, bool), String> {
    let mut names: Vec<String> = lock
        .packages
        .iter()
        .map(|package| package.name.clone())
        .collect();
    for tool in &lock.tools {
        for package in &tool.packages {
            names.push(package.name.clone());
        }
    }
    names.sort();
    names.dedup();
    let spinner = puv_core::Spinner::start(format!(
        "checking security advisories for {} packages",
        names.len()
    ));
    let advisories = if names.is_empty() {
        Vec::new()
    } else {
        registry.advisories(&names).map_err(|err| err.to_string())?
    };
    let mut by_package: HashMap<String, Vec<&Advisory>> = HashMap::new();
    for advisory in &advisories {
        by_package
            .entry(advisory.package.clone())
            .or_default()
            .push(advisory);
    }
    let constraints = direct_constraints(manifest);
    let mut upgrades = Vec::new();
    let mut outdated = Vec::new();
    let mut holes = Vec::new();
    let packages: Vec<_> = lock
        .packages
        .iter()
        .chain(lock.tools.iter().flat_map(|tool| tool.packages.iter()))
        .collect();
    let total = packages.len();
    for (index, package) in packages.into_iter().enumerate() {
        spinner.set(format!(
            "checking {} ({}/{})",
            package.name,
            index + 1,
            total
        ));
        let locked = Version::parse(&package.version).ok();
        if let Ok(releases) = registry.releases(&package.name) {
            let latest = releases
                .iter()
                .filter(|release| release.version.is_stable())
                .max_by(|left, right| left.version.cmp(&right.version));
            if let (Some(locked), Some(latest)) = (locked.as_ref(), latest)
                && latest.version > *locked
            {
                let spec = constraints.get(&package.name);
                let allowed = spec.and_then(|spec| Constraint::parse(spec).ok());
                if allowed
                    .as_ref()
                    .is_none_or(|constraint| constraint.matches(&latest.version))
                {
                    upgrades.push(format!(
                        "{}  {} {} -> {}",
                        tone("upgrade", "32"),
                        tone(&package.name, "36"),
                        tone(&package.version, "2"),
                        tone(&latest.version.to_string(), "32")
                    ));
                } else if let Some(spec) = spec {
                    outdated.push(format!(
                        "{}  {} {}  latest {} is outside {spec}",
                        tone("outdated", "33"),
                        tone(&package.name, "36"),
                        tone(&package.version, "2"),
                        tone(&latest.version.to_string(), "33")
                    ));
                }
            }
        }
        if let Some(found) = by_package.get(&package.name) {
            for advisory in found {
                if advisory_matches(advisory, &package.version) {
                    holes.push((package, *advisory));
                }
            }
        }
    }
    spinner.finish();
    let mut out = String::new();
    if holes.is_empty() && upgrades.is_empty() && outdated.is_empty() {
        out.push_str(&tone("no vulnerabilities", "32"));
        out.push('\n');
        out.push_str(&tone("no upgrades", "32"));
        out.push('\n');
        return Ok((out, false));
    }
    for (package, advisory) in &holes {
        out.push_str(&format!(
            "{} {}\n",
            tone(&package.name, "36"),
            tone(&package.version, "2")
        ));
        let severity = advisory.severity.as_deref().unwrap_or("unknown");
        let cve = advisory.cve.as_deref().unwrap_or(advisory.id.as_str());
        out.push_str(&format!(
            "  {}  {}  {}  {}\n",
            tone("vulnerability", "31"),
            tone(severity, "31"),
            tone(cve, "1;31"),
            advisory.title
        ));
        out.push_str(&format!(
            "  {}  {}\n",
            tone("affected", "33"),
            advisory.affected
        ));
        if let Some(link) = &advisory.link {
            out.push_str(&format!("  {}\n", tone(link, "36")));
        }
    }
    for line in upgrades.iter().chain(outdated.iter()) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!(
        "\n{} vulnerabilities, {} upgrades, {} outdated\n",
        holes.len(),
        upgrades.len(),
        outdated.len()
    ));
    Ok((out, !holes.is_empty()))
}

struct Node {
    label: String,
    note: String,
    children: Vec<Node>,
}

#[derive(Clone)]
struct WhyStep {
    name: String,
    version: String,
    constraint: String,
    kind: Kind,
}

fn package_node(
    packages: &BTreeMap<String, &LockedPackage>,
    edge: &RootEdge,
    seen: &mut BTreeSet<String>,
) -> Option<Node> {
    let package = packages.get(&edge.name)?;
    if !seen.insert(package.name.clone()) {
        return Some(Node {
            label: format!("{}@{}", package.name, package.version),
            note: edge.kind.label().trim().to_string(),
            children: Vec::new(),
        });
    }
    let mut children = Vec::new();
    for dependency in &package.dependencies {
        let child = RootEdge {
            name: dependency.clone(),
            constraint: String::new(),
            kind: Kind::Prod,
        };
        if let Some(node) = package_node(packages, &child, seen) {
            children.push(node);
        }
    }
    Some(Node {
        label: format!("{}@{}", package.name, package.version),
        note: edge.kind.label().trim().to_string(),
        children,
    })
}

fn push_tree(out: &mut String, prefix: &str, nodes: &[Node]) {
    for (index, node) in nodes.iter().enumerate() {
        let last = index + 1 == nodes.len();
        let branch = if last { "└─" } else { "├─" };
        out.push_str(prefix);
        out.push_str(branch);
        out.push(' ');
        out.push_str(&node.label);
        if !node.note.is_empty() {
            out.push_str(" (");
            out.push_str(&node.note);
            out.push(')');
        }
        out.push('\n');
        let next = format!("{prefix}{}", if last { "   " } else { "│  " });
        push_tree(out, &next, &node.children);
    }
}

fn walk_why(
    packages: &BTreeMap<String, &LockedPackage>,
    edge: &RootEdge,
    target: &str,
    stack: &mut Vec<WhyStep>,
    paths: &mut Vec<Vec<WhyStep>>,
) {
    let Some(package) = packages.get(&edge.name) else {
        return;
    };
    if stack.iter().any(|step| step.name == package.name) {
        return;
    }
    stack.push(WhyStep {
        name: package.name.clone(),
        version: package.version.clone(),
        constraint: edge.constraint.clone(),
        kind: edge.kind,
    });
    if package.name == target {
        paths.push(stack.clone());
    } else {
        for dependency in &package.dependencies {
            let child = RootEdge {
                name: dependency.clone(),
                constraint: String::new(),
                kind: Kind::Prod,
            };
            walk_why(packages, &child, target, stack, paths);
        }
    }
    stack.pop();
}

fn root_edges(manifest: &Manifest) -> Vec<RootEdge> {
    let mut edges = Vec::new();
    for (name, constraint) in &manifest.dependencies {
        edges.push(RootEdge {
            name: name.clone(),
            constraint: constraint.clone(),
            kind: Kind::Prod,
        });
    }
    for (name, constraint) in &manifest.dev_dependencies {
        edges.push(RootEdge {
            name: name.clone(),
            constraint: constraint.clone(),
            kind: Kind::Dev,
        });
    }
    for (name, constraint) in &manifest.tool_dependencies {
        edges.push(RootEdge {
            name: name.clone(),
            constraint: constraint.clone(),
            kind: Kind::Tool,
        });
    }
    edges
}

fn direct_constraints(manifest: &Manifest) -> BTreeMap<String, String> {
    root_edges(manifest)
        .into_iter()
        .map(|edge| (edge.name, edge.constraint))
        .collect()
}

fn index_packages(packages: &[LockedPackage]) -> BTreeMap<String, &LockedPackage> {
    packages
        .iter()
        .map(|package| (package.name.clone(), package))
        .collect()
}

fn split_spec(query: &str) -> Result<(String, Option<String>), String> {
    let query = query.trim();
    let (name, version) = if let Some((name, version)) = query.split_once('@') {
        (name, Some(version.trim().to_string()))
    } else {
        (query, None)
    };
    let name = puv_core::normalize_name(name);
    if !name.contains('/') {
        return Err(format!("'{query}' is not a vendor/package name"));
    }
    Ok((name, version.filter(|version| !version.is_empty())))
}

fn advisory_matches(advisory: &Advisory, version: &str) -> bool {
    let Ok(version) = Version::parse(version) else {
        return false;
    };
    Constraint::parse(&advisory.affected)
        .map(|constraint| constraint.matches(&version))
        .unwrap_or(true)
}
