use std::fmt;

use crate::{Error, Result};

/// Composer stability, ordered from least to most stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stability {
    Dev,
    Alpha,
    Beta,
    Rc,
    Stable,
}

impl Stability {
    pub fn parse(name: &str) -> Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "dev" => Ok(Self::Dev),
            "alpha" | "a" => Ok(Self::Alpha),
            "beta" | "b" => Ok(Self::Beta),
            "rc" => Ok(Self::Rc),
            "stable" => Ok(Self::Stable),
            other => Err(Error::new(format!("unknown stability '{other}'"))),
        }
    }
}

impl fmt::Display for Stability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Dev => "dev",
            Self::Alpha => "alpha",
            Self::Beta => "beta",
            Self::Rc => "RC",
            Self::Stable => "stable",
        })
    }
}

/// A Composer version: four numeric components plus stability.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub extra: u64,
    pub stability: Stability,
    pub stability_num: u64,
}

impl Version {
    pub fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            extra: 0,
            stability: Stability::Stable,
            stability_num: 0,
        }
    }

    pub fn parse(input: &str) -> Result<Self> {
        let raw = input.trim();
        if raw.is_empty() {
            return Err(Error::new("empty version"));
        }
        let raw = raw
            .strip_prefix('v')
            .or_else(|| raw.strip_prefix('V'))
            .unwrap_or(raw);
        let raw = raw.split_once('+').map(|(v, _)| v).unwrap_or(raw);
        if raw.contains('*') {
            return Err(Error::new(format!(
                "'{input}' is a wildcard, not a version"
            )));
        }
        let (numeric, stability, stability_num) = split_stability(raw)?;
        let mut parts = Vec::new();
        for part in numeric.split('.') {
            if part.is_empty() {
                return Err(Error::new(format!("invalid version '{input}'")));
            }
            let n: u64 = part
                .parse()
                .map_err(|_| Error::new(format!("invalid version '{input}'")))?;
            parts.push(n);
        }
        if parts.is_empty() || parts.len() > 4 {
            return Err(Error::new(format!("invalid version '{input}'")));
        }
        while parts.len() < 4 {
            parts.push(0);
        }
        Ok(Self {
            major: parts[0],
            minor: parts[1],
            patch: parts[2],
            extra: parts[3],
            stability,
            stability_num,
        })
    }

    pub fn is_stable(&self) -> bool {
        self.stability == Stability::Stable
    }

    /// Number of numeric components in a token such as `1.2` or `1.2.3-RC1`.
    pub fn component_count(token: &str) -> usize {
        let numeric = token
            .split_once('-')
            .map(|(n, _)| n)
            .unwrap_or(token)
            .split_once('+')
            .map(|(n, _)| n)
            .unwrap_or(token);
        let numeric = numeric
            .strip_prefix('v')
            .or_else(|| numeric.strip_prefix('V'))
            .unwrap_or(numeric);
        numeric.split('.').filter(|p| !p.is_empty()).count()
    }

    pub fn bump_component(&self, index: usize) -> Self {
        let mut comps = [self.major, self.minor, self.patch, self.extra];
        comps[index] = comps[index].saturating_add(1);
        for slot in comps.iter_mut().skip(index + 1) {
            *slot = 0;
        }
        Self {
            major: comps[0],
            minor: comps[1],
            patch: comps[2],
            extra: comps[3],
            stability: Stability::Stable,
            stability_num: 0,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if self.extra != 0 {
            write!(f, ".{}", self.extra)?;
        }
        if self.stability != Stability::Stable {
            write!(f, "-{}", self.stability)?;
            if self.stability_num > 0 {
                write!(f, "{}", self.stability_num)?;
            }
        }
        Ok(())
    }
}

fn split_stability(input: &str) -> Result<(&str, Stability, u64)> {
    let Some((numeric, rest)) = input.split_once('-') else {
        return Ok((input, Stability::Stable, 0));
    };
    if numeric.is_empty() {
        return Err(Error::new(format!("invalid version '{input}'")));
    }
    let (stability, num) = parse_stability_suffix(rest)?;
    Ok((numeric, stability, num))
}

fn parse_stability_suffix(rest: &str) -> Result<(Stability, u64)> {
    let rest = rest.trim().to_ascii_lowercase();
    if rest.is_empty() {
        return Err(Error::new("missing stability suffix"));
    }
    let split = rest.find(|c: char| c.is_ascii_digit() || c == '.');
    let (name, num_src) = if let Some(idx) = split {
        (&rest[..idx], &rest[idx..])
    } else {
        (rest.as_str(), "")
    };
    let name = name.trim_end_matches('.');
    let stability = Stability::parse(name)?;
    let num_src = num_src.trim_start_matches('.');
    if num_src.is_empty() {
        return Ok((stability, 0));
    }
    let num: u64 = num_src
        .parse()
        .map_err(|_| Error::new(format!("invalid stability number '{rest}'")))?;
    Ok((stability, num))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_orders() {
        let plain = Version::parse("v1.2.3").unwrap();
        assert_eq!(plain, Version::parse("1.2.3").unwrap());
        assert_eq!(plain.to_string(), "1.2.3");

        let alpha = Version::parse("7.0.0-alpha1").unwrap();
        let beta = Version::parse("7.0.0-beta1").unwrap();
        let rc = Version::parse("7.0.0-RC1").unwrap();
        let stable = Version::parse("7.0.0").unwrap();
        let next = Version::parse("7.0.1").unwrap();
        assert!(alpha < beta);
        assert!(beta < rc);
        assert!(rc < stable);
        assert!(stable < next);
        assert!(Version::parse("1.0.0").unwrap() < Version::parse("1.0.1-beta1").unwrap());
    }
}
