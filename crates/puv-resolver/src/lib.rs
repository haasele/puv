//! PubGrub dependency resolver for Composer package metadata.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use pubgrub::{
    DefaultStringReporter, Dependencies, DependencyConstraints, DependencyProvider, PubGrubError,
    Ranges, Reporter, SelectedDependencies, resolve,
};
use puv_core::normalize_name;
use puv_registry::{MetadataProvider, PackageRelease};
use puv_semver::{Bound as VersionBound, Constraint, Stability, Version};

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("{message}")]
pub struct Error {
    message: String,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
struct SolveErr(String);

impl std::fmt::Display for SolveErr {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SolveErr {}

fn solve_err(err: impl std::fmt::Display) -> SolveErr {
    SolveErr(err.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum PackageId {
    Root,
    Named(String),
}

impl std::fmt::Display for PackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Root => f.write_str("project"),
            Self::Named(name) => f.write_str(name),
        }
    }
}

#[derive(Clone, Debug)]
struct Candidate {
    version: Version,
    release: Option<PackageRelease>,
    /// Concrete package that satisfies this virtual version.
    provider: Option<(String, Version)>,
}

pub struct SolveRequest<'a> {
    pub root_deps: BTreeMap<String, String>,
    pub registry: &'a dyn MetadataProvider,
    pub preferred: BTreeMap<String, Version>,
    pub runtime: Version,
    pub extensions: BTreeSet<String>,
}

pub fn solve(request: SolveRequest<'_>) -> Result<Vec<PackageRelease>> {
    let mut forbidden: HashSet<(String, Version)> = HashSet::new();
    let mut seen = HashSet::new();
    for _ in 0..32 {
        let provider = SolverProvider {
            request: &request,
            forbidden: &forbidden,
            cache: RefCell::new(HashMap::new()),
            provides: RefCell::new(HashMap::new()),
            indexed: RefCell::new(false),
        };
        match resolve(&provider, PackageId::Root, root_version()) {
            Ok(solution) => {
                let selected = materialize(&provider, &solution)?;
                if let Some(conflict) = first_conflict(&selected) {
                    if !seen.insert(conflict.clone()) {
                        return Err(Error::new(format!(
                            "{} {} remains in conflict with another selected package",
                            conflict.0, conflict.1
                        )));
                    }
                    forbidden.insert(conflict);
                    continue;
                }
                return Ok(selected);
            }
            Err(PubGrubError::NoSolution(mut tree)) => {
                tree.collapse_no_versions();
                return Err(Error::new(DefaultStringReporter::report(&tree)));
            }
            Err(err) => return Err(Error::new(err.to_string())),
        }
    }
    Err(Error::new("dependency resolution did not converge"))
}

struct Provide {
    version: Version,
    package: String,
    package_version: Version,
}

struct SolverProvider<'a> {
    request: &'a SolveRequest<'a>,
    forbidden: &'a HashSet<(String, Version)>,
    cache: RefCell<HashMap<String, Vec<PackageRelease>>>,
    provides: RefCell<HashMap<String, Vec<Provide>>>,
    indexed: RefCell<bool>,
}

impl DependencyProvider for SolverProvider<'_> {
    type Err = SolveErr;
    type P = PackageId;
    type V = Version;
    type VS = Ranges<Version>;
    type M = String;
    type Priority = i64;

    fn choose_version(
        &self,
        package: &Self::P,
        range: &Self::VS,
    ) -> std::result::Result<Option<Self::V>, Self::Err> {
        let PackageId::Named(name) = package else {
            return Ok(range.contains(&root_version()).then(root_version));
        };
        let candidates = self.candidates(name)?;
        if let Some(preferred) = self.request.preferred.get(name)
            && range.contains(preferred)
            && candidates
                .iter()
                .any(|candidate| &candidate.version == preferred)
        {
            return Ok(Some(preferred.clone()));
        }
        Ok(candidates
            .into_iter()
            .filter(|candidate| {
                range.contains(&candidate.version)
                    && candidate.version.stability >= Stability::Stable
            })
            .map(|candidate| candidate.version)
            .max())
    }

    fn prioritize(
        &self,
        package: &Self::P,
        _range: &Self::VS,
        _stats: &pubgrub::PackageResolutionStatistics,
    ) -> Self::Priority {
        match package {
            PackageId::Root => i64::MAX,
            PackageId::Named(name) if is_platform(name) => i64::MAX - 1,
            PackageId::Named(name) => {
                let count = self
                    .candidates(name)
                    .map(|items| items.len())
                    .unwrap_or(10_000) as i64;
                1_000_000 - count
            }
        }
    }

    fn get_dependencies(
        &self,
        package: &Self::P,
        version: &Self::V,
    ) -> std::result::Result<Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        match package {
            PackageId::Root => Ok(Dependencies::Available(
                translate(&self.request.root_deps).map_err(solve_err)?,
            )),
            PackageId::Named(name) => {
                let candidates = self.candidates(name).map_err(solve_err)?;
                let Some(candidate) = candidates.into_iter().find(|candidate| {
                    &candidate.version == version
                        && (candidate.release.is_some()
                            || candidate.provider.is_some()
                            || is_platform(name))
                }) else {
                    return Ok(Dependencies::Unavailable(format!(
                        "version {version} of {name} is unavailable"
                    )));
                };
                if let Some(release) = &candidate.release {
                    return Ok(Dependencies::Available(
                        translate(&release.dependencies).map_err(solve_err)?,
                    ));
                }
                if let Some((provider, provider_version)) = &candidate.provider {
                    let deps = DependencyConstraints::from_iter([(
                        PackageId::Named(provider.clone()),
                        Ranges::singleton(provider_version.clone()),
                    )]);
                    return Ok(Dependencies::Available(deps));
                }
                Ok(Dependencies::Available(DependencyConstraints::default()))
            }
        }
    }
}

impl SolverProvider<'_> {
    fn candidates(&self, name: &str) -> std::result::Result<Vec<Candidate>, SolveErr> {
        self.index_known()?;
        let mut found = Vec::new();
        if name == "php" {
            found.push(self.platform_candidate(self.request.runtime.clone()));
        } else if let Some(extension) = name.strip_prefix("ext-") {
            if self
                .request
                .extensions
                .iter()
                .any(|present| present.eq_ignore_ascii_case(extension))
            {
                found.push(self.platform_candidate(self.request.runtime.clone()));
            }
        } else if name.starts_with("lib-") {
            found.push(self.platform_candidate(Version::parse("999.0.0").map_err(solve_err)?));
        } else if name == "composer-plugin-api" || name == "composer-runtime-api" {
            found.push(self.platform_candidate(Version::parse("2.99.0").map_err(solve_err)?));
        } else if !is_platform(name) {
            for release in self.load(name)? {
                if release.version.stability < Stability::Stable {
                    continue;
                }
                if self
                    .forbidden
                    .contains(&(name.to_string(), release.version.clone()))
                {
                    continue;
                }
                found.push(Candidate {
                    version: release.version.clone(),
                    release: Some(release),
                    provider: None,
                });
            }
        }
        if let Some(providers) = self.provides.borrow().get(name) {
            for provide in providers {
                if self
                    .forbidden
                    .contains(&(provide.package.clone(), provide.package_version.clone()))
                    || self
                        .forbidden
                        .contains(&(name.to_string(), provide.version.clone()))
                {
                    continue;
                }
                found.push(Candidate {
                    version: provide.version.clone(),
                    release: None,
                    provider: Some((provide.package.clone(), provide.package_version.clone())),
                });
            }
        }
        Ok(found)
    }

    fn platform_candidate(&self, version: Version) -> Candidate {
        Candidate {
            version,
            release: None,
            provider: None,
        }
    }

    fn index_known(&self) -> std::result::Result<(), SolveErr> {
        if *self.indexed.borrow() {
            return Ok(());
        }
        *self.indexed.borrow_mut() = true;
        if let Some(names) = self.request.registry.known_names() {
            for name in names {
                self.load(&name)?;
            }
        }
        Ok(())
    }

    fn load(&self, name: &str) -> std::result::Result<Vec<PackageRelease>, SolveErr> {
        let name = normalize_name(name);
        if let Some(hit) = self.cache.borrow().get(&name).cloned() {
            return Ok(hit);
        }
        let releases = self.request.registry.releases(&name).map_err(solve_err)?;
        for release in &releases {
            self.note_provides(release);
        }
        self.cache.borrow_mut().insert(name, releases.clone());
        Ok(releases)
    }

    fn note_provides(&self, release: &PackageRelease) {
        let mut map = self.provides.borrow_mut();
        for (name, spec) in release.provides.iter().chain(release.replaces.iter()) {
            if name == "php" || name.starts_with("composer-") {
                continue;
            }
            let Some(version) = provided_version(spec, &release.version) else {
                continue;
            };
            map.entry(normalize_name(name)).or_default().push(Provide {
                version,
                package: release.name.clone(),
                package_version: release.version.clone(),
            });
        }
    }
}

fn provided_version(spec: &str, release: &Version) -> Option<Version> {
    if spec == "self.version" || spec == "*" {
        return Some(release.clone());
    }
    if let Ok(version) = Version::parse(spec) {
        return Some(version);
    }
    let constraint = Constraint::parse(spec).ok()?;
    constraint.matches(release).then(|| release.clone())
}

fn translate(
    dependencies: &BTreeMap<String, String>,
) -> std::result::Result<DependencyConstraints<PackageId, Ranges<Version>>, SolveErr> {
    let mut pairs = Vec::new();
    for (name, spec) in dependencies {
        let constraint = Constraint::parse(spec).map_err(solve_err)?;
        pairs.push((
            PackageId::Named(normalize_name(name)),
            constraint_to_ranges(&constraint),
        ));
    }
    Ok(DependencyConstraints::from_iter(pairs))
}

fn constraint_to_ranges(constraint: &Constraint) -> Ranges<Version> {
    let mut combined = Ranges::empty();
    for atom in &constraint.atoms {
        let mut range = Ranges::<Version>::full();
        if let Some(bound) = &atom.lower {
            range = range.intersection(&bound_to_lower(bound));
        }
        if let Some(bound) = &atom.upper {
            range = range.intersection(&bound_to_upper(bound));
        }
        for excluded in &atom.exclude {
            range = range.intersection(&Ranges::singleton(excluded.clone()).complement());
        }
        combined = combined.union(&range);
    }
    combined
}

fn bound_to_lower(bound: &VersionBound) -> Ranges<Version> {
    if bound.inclusive {
        Ranges::higher_than(bound.version.clone())
    } else {
        Ranges::strictly_higher_than(bound.version.clone())
    }
}

fn bound_to_upper(bound: &VersionBound) -> Ranges<Version> {
    if bound.inclusive {
        Ranges::lower_than(bound.version.clone())
    } else {
        Ranges::strictly_lower_than(bound.version.clone())
    }
}

fn materialize(
    provider: &SolverProvider<'_>,
    solution: &SelectedDependencies<PackageId, Version>,
) -> Result<Vec<PackageRelease>> {
    let mut selected = Vec::new();
    for (package, version) in solution.iter() {
        let PackageId::Named(name) = package else {
            continue;
        };
        if is_platform(name) {
            continue;
        }
        let candidates = provider
            .candidates(name)
            .map_err(|error| Error::new(error.to_string()))?;
        if let Some(release) = candidates.into_iter().find_map(|candidate| {
            (candidate.version == *version)
                .then_some(candidate.release)
                .flatten()
        }) {
            selected.push(release);
        }
    }
    selected.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.version.cmp(&right.version))
    });
    Ok(selected)
}

fn first_conflict(selected: &[PackageRelease]) -> Option<(String, Version)> {
    let by_name: HashMap<&str, &PackageRelease> = selected
        .iter()
        .map(|release| (release.name.as_str(), release))
        .collect();
    for release in selected {
        for (other, spec) in release.conflicts.iter().chain(release.replaces.iter()) {
            let Some(found) = by_name.get(other.as_str()) else {
                continue;
            };
            let matches = Constraint::parse(spec)
                .map(|constraint| constraint.matches(&found.version))
                .unwrap_or(true);
            if matches {
                return Some((found.name.clone(), found.version.clone()));
            }
        }
    }
    None
}

fn root_version() -> Version {
    Version::new(0, 0, 0)
}

fn is_platform(name: &str) -> bool {
    name == "php"
        || name.starts_with("ext-")
        || name.starts_with("lib-")
        || name == "composer-plugin-api"
        || name == "composer-runtime-api"
}

#[cfg(test)]
mod tests {
    use super::*;
    use puv_registry::MemoryRegistry;
    use std::collections::HashMap;

    fn release(name: &str, version: &str, dependencies: &[(&str, &str)]) -> PackageRelease {
        PackageRelease {
            name: name.to_string(),
            version: Version::parse(version).unwrap(),
            dependencies: dependencies
                .iter()
                .map(|(name, spec)| (name.to_string(), spec.to_string()))
                .collect(),
            provides: BTreeMap::new(),
            replaces: BTreeMap::new(),
            conflicts: BTreeMap::new(),
            autoload: puv_core::Autoload::default(),
            bins: Vec::new(),
            dist: None,
            description: None,
            homepage: None,
            licenses: Vec::new(),
            keywords: Vec::new(),
            authors: Vec::new(),
            published: None,
            package_type: None,
        }
    }

    fn registry(packages: Vec<PackageRelease>) -> MemoryRegistry {
        let mut map: HashMap<String, Vec<PackageRelease>> = HashMap::new();
        for package in packages {
            map.entry(package.name.clone()).or_default().push(package);
        }
        MemoryRegistry { packages: map }
    }

    fn solve_root(
        registry: &MemoryRegistry,
        deps: &[(&str, &str)],
        preferred: &[(&str, &str)],
    ) -> Result<Vec<PackageRelease>> {
        solve(SolveRequest {
            root_deps: deps
                .iter()
                .map(|(name, spec)| ((*name).to_string(), (*spec).to_string()))
                .collect(),
            registry,
            preferred: preferred
                .iter()
                .map(|(name, version)| (name.to_string(), Version::parse(version).unwrap()))
                .collect(),
            runtime: Version::parse("8.4.23").unwrap(),
            extensions: [
                "mbstring",
                "json",
                "ctype",
                "phar",
                "tokenizer",
                "xml",
                "dom",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        })
    }

    #[test]
    fn picks_the_highest_matching_stable_release() {
        let registry = registry(vec![
            release("acme/lib", "1.0.0", &[("php", ">=8.1")]),
            release("acme/lib", "1.2.0", &[("php", ">=8.1")]),
            release("acme/lib", "1.3.0-beta1", &[("php", ">=8.1")]),
            release("acme/lib", "2.0.0", &[("php", ">=8.1")]),
        ]);
        let solved = solve_root(&registry, &[("acme/lib", "^1.0")], &[]).unwrap();
        assert_eq!(solved.len(), 1);
        assert_eq!(solved[0].version, Version::parse("1.2.0").unwrap());
    }

    #[test]
    fn prefers_the_locked_version() {
        let registry = registry(vec![
            release("acme/lib", "1.0.0", &[]),
            release("acme/lib", "1.2.0", &[]),
        ]);
        let solved =
            solve_root(&registry, &[("acme/lib", "^1.0")], &[("acme/lib", "1.0.0")]).unwrap();
        assert_eq!(solved[0].version, Version::parse("1.0.0").unwrap());
    }

    #[test]
    fn reports_a_conflict_chain() {
        let registry = registry(vec![
            release("acme/left", "1.0.0", &[("acme/lib", "^1.0")]),
            release("acme/right", "1.0.0", &[("acme/lib", "^2.0")]),
            release("acme/lib", "1.5.0", &[]),
            release("acme/lib", "2.1.0", &[]),
        ]);
        let error = solve_root(
            &registry,
            &[("acme/left", "^1.0"), ("acme/right", "^1.0")],
            &[],
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("acme/lib") || message.contains("acme/left"),
            "{message}"
        );
    }

    #[test]
    fn selects_a_provider_for_a_virtual_package() {
        let mut implementation = release("logger/impl", "2.0.0", &[]);
        implementation
            .provides
            .insert("psr/log-implementation".to_string(), "1.0.0".to_string());
        let registry = registry(vec![implementation]);
        let solved = solve_root(&registry, &[("psr/log-implementation", "^1.0")], &[]).unwrap();
        assert_eq!(solved.len(), 1);
        assert_eq!(solved[0].name, "logger/impl");
    }

    #[test]
    fn rejects_a_missing_extension() {
        let registry = registry(vec![release("acme/lib", "1.0.0", &[("ext-missing", "*")])]);
        let error = solve_root(&registry, &[("acme/lib", "^1")], &[]).unwrap_err();
        assert!(error.to_string().contains("ext-missing"), "{error}");
    }
}
