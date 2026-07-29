use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use crate::errors::{ArkError, Result};

/// Represents a segment within a version (numeric or string).
#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionSegment {
    Number(u64),
    Str(String),
}

impl PartialOrd for VersionSegment {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VersionSegment {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (VersionSegment::Number(a), VersionSegment::Number(b)) => a.cmp(b),
            (VersionSegment::Str(a), VersionSegment::Str(b)) => a.cmp(b),
            // Numbers are considered greater than alphanumeric strings (e.g. 1 > alpha)
            (VersionSegment::Number(_), VersionSegment::Str(_)) => Ordering::Greater,
            (VersionSegment::Str(_), VersionSegment::Number(_)) => Ordering::Less,
        }
    }
}

/// Represents a parsed version string.
#[derive(Debug, Clone)]
pub struct Version {
    raw: String,
    segments: Vec<VersionSegment>,
}

impl Version {
    /// Parses a version string into a Version struct.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            return Err(ArkError::InvalidPackage("Empty version string".into()));
        }

        let mut segments = Vec::new();
        for part in s.split(&['.', '-', '_'][..]) {
            if part.is_empty() {
                continue;
            }
            if let Ok(num) = part.parse::<u64>() {
                segments.push(VersionSegment::Number(num));
            } else {
                segments.push(VersionSegment::Str(part.to_string()));
            }
        }

        if segments.is_empty() {
            return Err(ArkError::InvalidPackage(format!(
                "Invalid version string: {}",
                s
            )));
        }

        Ok(Self {
            raw: s.to_string(),
            segments,
        })
    }

    /// Returns the raw version string.
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let max_len = self.segments.len().max(other.segments.len());
        for i in 0..max_len {
            let seg1 = self.segments.get(i);
            let seg2 = other.segments.get(i);

            match (seg1, seg2) {
                (Some(s1), Some(s2)) => {
                    let ord = s1.cmp(s2);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                (Some(VersionSegment::Number(n)), None) => {
                    if *n != 0 {
                        return Ordering::Greater;
                    }
                }
                (Some(VersionSegment::Str(_)), None) => return Ordering::Less, // 2.0-alpha < 2.0
                (None, Some(VersionSegment::Number(n))) => {
                    if *n != 0 {
                        return Ordering::Less;
                    }
                }
                (None, Some(VersionSegment::Str(_))) => return Ordering::Greater, // 2.0 > 2.0-alpha
                (None, None) => unreachable!(),
            }
        }
        Ordering::Equal
    }
}

impl FromStr for Version {
    type Err = ArkError;

    fn from_str(s: &str) -> Result<Self> {
        Version::parse(s)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.raw)
    }
}

/// Version comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionOp {
    Equal,              // = or ==
    NotEqual,           // !=
    LessThan,           // <
    LessThanOrEqual,    // <=
    GreaterThan,        // >
    GreaterThanOrEqual, // >=
}

impl VersionOp {
    /// Evaluates if `v1 op v2` is true.
    pub fn eval(&self, v1: &Version, v2: &Version) -> bool {
        match self {
            VersionOp::Equal => v1 == v2,
            VersionOp::NotEqual => v1 != v2,
            VersionOp::LessThan => v1 < v2,
            VersionOp::LessThanOrEqual => v1 <= v2,
            VersionOp::GreaterThan => v1 > v2,
            VersionOp::GreaterThanOrEqual => v1 >= v2,
        }
    }
}

impl FromStr for VersionOp {
    type Err = ArkError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "=" | "==" => Ok(VersionOp::Equal),
            "!=" => Ok(VersionOp::NotEqual),
            "<" => Ok(VersionOp::LessThan),
            "<=" => Ok(VersionOp::LessThanOrEqual),
            ">" => Ok(VersionOp::GreaterThan),
            ">=" => Ok(VersionOp::GreaterThanOrEqual),
            _ => Err(ArkError::InvalidPackage(format!(
                "Invalid version operator: {}",
                s
            ))),
        }
    }
}

impl fmt::Display for VersionOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let op_str = match self {
            VersionOp::Equal => "==",
            VersionOp::NotEqual => "!=",
            VersionOp::LessThan => "<",
            VersionOp::LessThanOrEqual => "<=",
            VersionOp::GreaterThan => ">",
            VersionOp::GreaterThanOrEqual => ">=",
        };
        write!(f, "{}", op_str)
    }
}

/// Represents a requirement on a package dependency (name + op + version).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyReq {
    pub name: String,
    pub op: Option<VersionOp>,
    pub version: Option<Version>,
}

impl DependencyReq {
    /// Parses a string like `glibc>=2.41` or `bash` into a DependencyReq.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            return Err(ArkError::InvalidPackage(
                "Empty dependency requirement".into(),
            ));
        }

        let ops = ["==", "!=", "<=", ">=", "=", "<", ">"];
        for op_str in ops {
            if let Some(pos) = s.find(op_str) {
                let name = s[..pos].trim().to_string();
                let ver_str = s[pos + op_str.len()..].trim();
                let op = VersionOp::from_str(op_str)?;
                let version = Version::parse(ver_str)?;
                return Ok(Self {
                    name,
                    op: Some(op),
                    version: Some(version),
                });
            }
        }

        // No operator, just package name
        Ok(Self {
            name: s.to_string(),
            op: None,
            version: None,
        })
    }

    /// Checks if a given version satisfies this dependency requirement.
    pub fn is_satisfied_by(&self, version: &Version) -> bool {
        match (&self.op, &self.version) {
            (Some(op), Some(req_ver)) => op.eval(version, req_ver),
            _ => true,
        }
    }
}

impl fmt::Display for DependencyReq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.op, &self.version) {
            (Some(op), Some(ver)) => write!(f, "{}{}{}", self.name, op, ver),
            _ => write!(f, "{}", self.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_parsing_and_comparison() {
        let v1 = Version::parse("5.3.0").unwrap();
        let v2 = Version::parse("5.3").unwrap();
        let v3 = Version::parse("5.2.9").unwrap();
        let v4 = Version::parse("2.41").unwrap();
        let v5 = Version::parse("2.4.1").unwrap();

        assert_eq!(v1, v2);
        assert!(v1 > v3);
        assert!(v4 > v5); // 41 > 4
    }

    #[test]
    fn test_dependency_req_parsing() {
        let req = DependencyReq::parse("glibc>=2.41").unwrap();
        assert_eq!(req.name, "glibc");
        assert_eq!(req.op, Some(VersionOp::GreaterThanOrEqual));
        assert_eq!(req.version, Some(Version::parse("2.41").unwrap()));

        let ver_ok = Version::parse("2.42").unwrap();
        let ver_bad = Version::parse("2.40").unwrap();
        assert!(req.is_satisfied_by(&ver_ok));
        assert!(!req.is_satisfied_by(&ver_bad));
    }
}
