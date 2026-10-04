use std::cmp::Ordering;

use crate::version::{Stability, Version};
use crate::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bound {
    pub version: Version,
    pub inclusive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Atom {
    pub lower: Option<Bound>,
    pub upper: Option<Bound>,
    pub exclude: Vec<Version>,
    pub min_stability: Stability,
}

impl Atom {
    fn unbounded(min_stability: Stability) -> Self {
        Self {
            lower: None,
            upper: None,
            exclude: Vec::new(),
            min_stability,
        }
    }

    fn matches(&self, version: &Version) -> bool {
        if version.stability < self.min_stability {
            return false;
        }
        if let Some(low) = &self.lower {
            match version.cmp(&low.version) {
                Ordering::Less => return false,
                Ordering::Equal if !low.inclusive => return false,
                _ => {}
            }
        }
        if let Some(high) = &self.upper {
            match version.cmp(&high.version) {
                Ordering::Greater => return false,
                Ordering::Equal if !high.inclusive => return false,
                _ => {}
            }
        }
        !self.exclude.iter().any(|item| item == version)
    }
}

/// A Composer constraint. Atoms are combined with logical OR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Constraint {
    pub atoms: Vec<Atom>,
}

impl Constraint {
    pub fn any() -> Self {
        Self {
            atoms: vec![Atom::unbounded(Stability::Dev)],
        }
    }

    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() || input == "*" {
            return Ok(Self::any());
        }
        let mut atoms = Vec::new();
        for branch in split_or(input) {
            if branch.is_empty() {
                return Err(Error::new(format!("empty constraint in '{input}'")));
            }
            atoms.push(parse_and(branch)?);
        }
        Ok(Self { atoms })
    }

    pub fn matches(&self, version: &Version) -> bool {
        self.atoms.iter().any(|atom| atom.matches(version))
    }

    /// Caret constraint spanning the resolved release's compatible bucket.
    pub fn caret_bucket(version: &Version) -> Self {
        let rendered = if version.major > 0 {
            format!("^{}.{}", version.major, version.minor)
        } else if version.minor > 0 {
            format!("^0.{}", version.minor)
        } else {
            format!("^0.0.{}", version.patch)
        };
        Self::parse(&rendered).expect("caret bucket is valid")
    }
}

fn parse_and(input: &str) -> Result<Atom> {
    let mut acc: Option<Atom> = None;
    for piece in input.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        for primary in split_primaries(piece)? {
            let next = parse_primary(&primary)?;
            acc = Some(match acc {
                None => next,
                Some(current) => intersect(current, next),
            });
        }
    }
    acc.ok_or_else(|| Error::new(format!("empty constraint '{input}'")))
}

fn split_primaries(input: &str) -> Result<Vec<String>> {
    let parts: Vec<&str> = input.split_whitespace().collect();
    if parts.len() == 3 && parts[1] == "-" {
        return Ok(vec![input.to_string()]);
    }
    if parts.is_empty() {
        return Err(Error::new("empty constraint"));
    }
    Ok(parts.into_iter().map(str::to_string).collect())
}

fn parse_primary(raw: &str) -> Result<Atom> {
    let (body, flag) = split_flag(raw)?;
    let body = body.trim();
    if body.is_empty() {
        return Err(Error::new(format!("invalid constraint '{raw}'")));
    }
    if body == "*" {
        return Ok(finalize(Atom::unbounded(Stability::Dev), flag));
    }
    if let Some((left, right)) = body.split_once(" - ") {
        return hyphen(left.trim(), right.trim(), flag);
    }
    if let Some(rest) = body.strip_prefix('^') {
        return caret(rest.trim(), flag);
    }
    if let Some(rest) = body.strip_prefix('~') {
        return tilde(rest.trim(), flag);
    }
    if body.contains('*') {
        return wildcard(body, flag);
    }
    let (op, version_src) = split_operator(body);
    let version = Version::parse(version_src)?;
    let implied = implied_floor(version.stability, op);
    let atom = match op {
        Some(">=") => lower_only(version, true),
        Some(">") => lower_only(version, false),
        Some("<=") => upper_only(version, true),
        Some("<") => upper_only(version, false),
        Some("!=") => Atom {
            lower: None,
            upper: None,
            exclude: vec![version],
            min_stability: Stability::Stable,
        },
        Some("==" | "=") | None => exact(version),
        Some(other) => return Err(Error::new(format!("unknown operator '{other}'"))),
    };
    Ok(finalize(atom, flag.or(Some(implied_floor(implied, op)))))
}

fn implied_floor(stability: Stability, op: Option<&str>) -> Stability {
    if matches!(op, None | Some("==" | "=" | ">=" | ">" | "^" | "~")) {
        stability
    } else {
        Stability::Stable
    }
}

fn caret(token: &str, flag: Option<Stability>) -> Result<Atom> {
    let version = Version::parse(token)?;
    let upper = if version.major > 0 {
        version.bump_component(0)
    } else if version.minor > 0 {
        version.bump_component(1)
    } else if version.patch > 0 {
        version.bump_component(2)
    } else {
        version.bump_component(3)
    };
    let implied = version.stability;
    Ok(finalize(
        Atom {
            lower: Some(Bound {
                version,
                inclusive: true,
            }),
            upper: Some(Bound {
                version: upper,
                inclusive: false,
            }),
            exclude: Vec::new(),
            min_stability: Stability::Stable,
        },
        flag.or(Some(implied)),
    ))
}

fn tilde(token: &str, flag: Option<Stability>) -> Result<Atom> {
    let count = Version::component_count(token).max(1);
    let version = Version::parse(token)?;
    let index = if count <= 1 { 0 } else { count - 2 };
    let upper = version.bump_component(index);
    let implied = version.stability;
    Ok(finalize(
        Atom {
            lower: Some(Bound {
                version,
                inclusive: true,
            }),
            upper: Some(Bound {
                version: upper,
                inclusive: false,
            }),
            exclude: Vec::new(),
            min_stability: Stability::Stable,
        },
        flag.or(Some(implied)),
    ))
}

fn wildcard(token: &str, flag: Option<Stability>) -> Result<Atom> {
    let (numeric, _) = token.split_once('*').unwrap();
    let numeric = numeric.trim_end_matches('.');
    if numeric.is_empty() {
        return Ok(finalize(Atom::unbounded(Stability::Dev), flag));
    }
    let count = Version::component_count(numeric);
    let version = Version::parse(numeric)?;
    let upper = version.bump_component(count - 1);
    Ok(finalize(
        Atom {
            lower: Some(Bound {
                version,
                inclusive: true,
            }),
            upper: Some(Bound {
                version: upper,
                inclusive: false,
            }),
            exclude: Vec::new(),
            min_stability: Stability::Stable,
        },
        flag.or(Some(Stability::Stable)),
    ))
}

fn hyphen(left: &str, right: &str, flag: Option<Stability>) -> Result<Atom> {
    let lower = Version::parse(left)?;
    let count = Version::component_count(right);
    let right_version = Version::parse(right)?;
    let (upper, inclusive) = if count >= 3 {
        (right_version.clone(), true)
    } else {
        (right_version.bump_component(count - 1), false)
    };
    let implied = if lower.is_stable() && right_version.is_stable() {
        Stability::Stable
    } else {
        lower.stability.min(right_version.stability)
    };
    Ok(finalize(
        Atom {
            lower: Some(Bound {
                version: lower,
                inclusive: true,
            }),
            upper: Some(Bound {
                version: upper,
                inclusive,
            }),
            exclude: Vec::new(),
            min_stability: Stability::Stable,
        },
        flag.or(Some(implied)),
    ))
}

fn exact(version: Version) -> Atom {
    let stability = version.stability;
    Atom {
        lower: Some(Bound {
            version: version.clone(),
            inclusive: true,
        }),
        upper: Some(Bound {
            version,
            inclusive: true,
        }),
        exclude: Vec::new(),
        min_stability: stability,
    }
}

fn lower_only(version: Version, inclusive: bool) -> Atom {
    let stability = version.stability;
    Atom {
        lower: Some(Bound { version, inclusive }),
        upper: None,
        exclude: Vec::new(),
        min_stability: if stability == Stability::Stable {
            Stability::Stable
        } else {
            stability
        },
    }
}

fn upper_only(version: Version, inclusive: bool) -> Atom {
    Atom {
        lower: None,
        upper: Some(Bound { version, inclusive }),
        exclude: Vec::new(),
        min_stability: Stability::Stable,
    }
}

fn finalize(mut atom: Atom, flag: Option<Stability>) -> Atom {
    if let Some(min) = flag {
        atom.min_stability = min;
    }
    if atom.min_stability < Stability::Stable {
        if let Some(low) = &mut atom.lower
            && low.inclusive
            && low.version.stability == Stability::Stable
        {
            low.version.stability = atom.min_stability;
            low.version.stability_num = 0;
        }
        if let Some(high) = &mut atom.upper
            && !high.inclusive
            && high.version.stability == Stability::Stable
        {
            high.version.stability = Stability::Dev;
            high.version.stability_num = 0;
        }
    }
    atom
}

fn intersect(mut left: Atom, right: Atom) -> Atom {
    left.lower = stricter_lower(left.lower, right.lower);
    left.upper = stricter_upper(left.upper, right.upper);
    left.exclude.extend(right.exclude);
    if right.min_stability > left.min_stability {
        left.min_stability = right.min_stability;
    }
    left
}

fn stricter_lower(a: Option<Bound>, b: Option<Bound>) -> Option<Bound> {
    match (a, b) {
        (None, other) | (other, None) => other,
        (Some(a), Some(b)) => Some(match a.version.cmp(&b.version) {
            Ordering::Greater => a,
            Ordering::Less => b,
            Ordering::Equal => {
                if a.inclusive {
                    b
                } else {
                    a
                }
            }
        }),
    }
}

fn stricter_upper(a: Option<Bound>, b: Option<Bound>) -> Option<Bound> {
    match (a, b) {
        (None, other) | (other, None) => other,
        (Some(a), Some(b)) => Some(match a.version.cmp(&b.version) {
            Ordering::Less => a,
            Ordering::Greater => b,
            Ordering::Equal => {
                if a.inclusive {
                    b
                } else {
                    a
                }
            }
        }),
    }
}

fn split_or(input: &str) -> Vec<&str> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'|' {
            out.push(input[start..index].trim());
            index += if index + 1 < bytes.len() && bytes[index + 1] == b'|' {
                2
            } else {
                1
            };
            start = index;
        } else {
            index += 1;
        }
    }
    out.push(input[start..].trim());
    out
}

fn split_flag(input: &str) -> Result<(&str, Option<Stability>)> {
    let Some((body, flag)) = input.rsplit_once('@') else {
        return Ok((input, None));
    };
    if flag.contains(' ') {
        return Ok((input, None));
    }
    Ok((body, Some(Stability::parse(flag)?)))
}

fn split_operator(input: &str) -> (Option<&str>, &str) {
    for op in [">=", "<=", "!=", "==", ">", "<", "="] {
        if let Some(rest) = input.strip_prefix(op) {
            return (Some(op), rest.trim());
        }
    }
    (None, input.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(constraint: &str, version: &str) -> bool {
        let constraint = Constraint::parse(constraint).unwrap();
        let version = Version::parse(version).unwrap();
        constraint.matches(&version)
    }

    #[test]
    fn composer_corpus() {
        let cases = [
            ("^7.0", "7.3.0", true),
            ("^7.0", "7.0.0", true),
            ("^7.0", "8.0.0", false),
            ("^7.0", "6.9.0", false),
            ("^7.0", "7.4.0-beta1", false),
            ("^0.3.2", "0.3.9", true),
            ("^0.3.2", "0.4.0", false),
            ("^0.0.3", "0.0.3", true),
            ("^0.0.3", "0.0.4", false),
            ("~7.0.1", "7.0.9", true),
            ("~7.0.1", "7.1.0", false),
            ("~7.0", "7.9.0", true),
            ("~7.0", "8.0.0", false),
            (">=8.1 <8.5", "8.4.23", true),
            (">=8.1 <8.5", "8.5.0", false),
            (">=8.1,<8.5", "8.1.0", true),
            ("1.2.*", "1.2.9", true),
            ("1.2.*", "1.3.0", false),
            ("^1.0 || ^2.0", "1.5.0", true),
            ("^1.0 || ^2.0", "2.1.0", true),
            ("^1.0 || ^2.0", "3.0.0", false),
            ("!=1.2.3", "1.2.4", true),
            ("!=1.2.3", "1.2.3", false),
            ("1.0 - 2.0", "2.0.9", true),
            ("1.0 - 2.0", "2.1.0", false),
            ("1.0.0 - 2.1.0", "2.1.0", true),
            ("1.0.0 - 2.1.0", "2.1.1", false),
            ("*", "0.0.1", true),
            ("7.3.0", "7.3.0", true),
            ("7.3.0", "7.3.1", false),
            ("==7.3.0", "7.3.0", true),
            ("^2.0@RC", "2.0.0-RC1", true),
            ("^2.0@RC", "2.1.0", true),
            ("^2.0@RC", "2.0.0-beta1", false),
            ("^2.0", "2.0.0-RC1", false),
            ("^1.2.3-RC1", "1.2.3-RC1", true),
            ("^1.2.3-RC1", "1.9.0", true),
            ("^1.2.3-RC1", "1.2.3-beta1", false),
            ("^1.2.3-RC1", "2.0.0-RC1", false),
        ];
        for (constraint, version, expected) in cases {
            assert_eq!(
                matches(constraint, version),
                expected,
                "{constraint} vs {version}"
            );
        }
    }
}
