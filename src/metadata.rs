use std::collections::HashSet;
use std::fmt;

use crate::errors::{ArkError, Result};
use crate::version::{DependencyReq, Version};

/// Represents a provided feature or alias, e.g., `bash=/usr/bin/bash`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvideItem {
    pub name: String,
    pub path: String,
}

impl ProvideItem {
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if let Some((name, path)) = s.split_once('=') {
            Ok(Self {
                name: name.trim().to_string(),
                path: path.trim().to_string(),
            })
        } else {
            Ok(Self {
                name: s.to_string(),
                path: String::new(),
            })
        }
    }
}

impl fmt::Display for ProvideItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.name)
        } else {
            write!(f, "{}={}", self.name, self.path)
        }
    }
}

/// Package metadata loaded from an `ARKPKG` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub name: String,
    pub description: String,
    pub version: Version,
    pub arch: String,
    pub url: Option<String>,
    pub license: Option<String>,
    pub maintainer: Option<String>,
    pub dependencies: Vec<DependencyReq>,
    pub provides: Vec<ProvideItem>,
    pub conflicts: Vec<String>,
}

impl Metadata {
    /// Parses an `ARKPKG` plain text file content into a Metadata struct.
    pub fn parse(content: &str) -> Result<Self> {
        let mut name = None;
        let mut description = None;
        let mut version = None;
        let mut arch = None;
        let mut url = None;
        let mut license = None;
        let mut maintainer = None;
        let mut dependencies = Vec::new();
        let mut provides = Vec::new();
        let mut conflicts = Vec::new();

        let mut seen_fields = HashSet::new();
        let mut current_section: Option<&str> = None;

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            // Check if line is a header field like `Name:`
            if let Some((key, val)) = trimmed.split_once(':') {
                let field_name = key.trim();
                let field_val = val.trim();

                // If key is a standard field
                match field_name {
                    "Name" => {
                        if !seen_fields.insert("Name") {
                            return Err(ArkError::InvalidPackage("Duplicate field: Name".into()));
                        }
                        if field_val.is_empty() {
                            return Err(ArkError::InvalidPackage("Empty Name field".into()));
                        }
                        name = Some(field_val.to_string());
                        current_section = None;
                    }
                    "Description" => {
                        if !seen_fields.insert("Description") {
                            return Err(ArkError::InvalidPackage(
                                "Duplicate field: Description".into(),
                            ));
                        }
                        description = Some(field_val.to_string());
                        current_section = None;
                    }
                    "Version" => {
                        if !seen_fields.insert("Version") {
                            return Err(ArkError::InvalidPackage(
                                "Duplicate field: Version".into(),
                            ));
                        }
                        version = Some(Version::parse(field_val)?);
                        current_section = None;
                    }
                    "Arch" => {
                        if !seen_fields.insert("Arch") {
                            return Err(ArkError::InvalidPackage("Duplicate field: Arch".into()));
                        }
                        if field_val.is_empty() {
                            return Err(ArkError::InvalidPackage("Empty Arch field".into()));
                        }
                        arch = Some(field_val.to_string());
                        current_section = None;
                    }
                    "URL" => {
                        if !seen_fields.insert("URL") {
                            return Err(ArkError::InvalidPackage("Duplicate field: URL".into()));
                        }
                        url = Some(field_val.to_string());
                        current_section = None;
                    }
                    "License" => {
                        if !seen_fields.insert("License") {
                            return Err(ArkError::InvalidPackage(
                                "Duplicate field: License".into(),
                            ));
                        }
                        license = Some(field_val.to_string());
                        current_section = None;
                    }
                    "Maintainer" => {
                        if !seen_fields.insert("Maintainer") {
                            return Err(ArkError::InvalidPackage(
                                "Duplicate field: Maintainer".into(),
                            ));
                        }
                        maintainer = Some(field_val.to_string());
                        current_section = None;
                    }
                    "Dependencies" => {
                        if !seen_fields.insert("Dependencies") {
                            return Err(ArkError::InvalidPackage(
                                "Duplicate field: Dependencies".into(),
                            ));
                        }
                        current_section = Some("Dependencies");
                        if !field_val.is_empty() {
                            dependencies.push(DependencyReq::parse(field_val)?);
                        }
                    }
                    "Provides" => {
                        if !seen_fields.insert("Provides") {
                            return Err(ArkError::InvalidPackage(
                                "Duplicate field: Provides".into(),
                            ));
                        }
                        current_section = Some("Provides");
                        if !field_val.is_empty() {
                            provides.push(ProvideItem::parse(field_val)?);
                        }
                    }
                    "Conflicts" => {
                        if !seen_fields.insert("Conflicts") {
                            return Err(ArkError::InvalidPackage(
                                "Duplicate field: Conflicts".into(),
                            ));
                        }
                        current_section = Some("Conflicts");
                        if !field_val.is_empty() {
                            conflicts.push(field_val.to_string());
                        }
                    }
                    _ => {
                        // Unknown field or value belonging to multi-line section containing a colon
                        if let Some(sec) = current_section {
                            match sec {
                                "Dependencies" => dependencies.push(DependencyReq::parse(trimmed)?),
                                "Provides" => provides.push(ProvideItem::parse(trimmed)?),
                                "Conflicts" => conflicts.push(trimmed.to_string()),
                                _ => {}
                            }
                        } else {
                            return Err(ArkError::InvalidPackage(format!(
                                "Unknown field: {}",
                                field_name
                            )));
                        }
                    }
                }
            } else if let Some(sec) = current_section {
                // Continuation line for multi-line sections
                match sec {
                    "Dependencies" => dependencies.push(DependencyReq::parse(trimmed)?),
                    "Provides" => provides.push(ProvideItem::parse(trimmed)?),
                    "Conflicts" => conflicts.push(trimmed.to_string()),
                    _ => {}
                }
            } else {
                return Err(ArkError::InvalidPackage(format!(
                    "Unexpected metadata line: {}",
                    trimmed
                )));
            }
        }

        let name = name.ok_or_else(|| ArkError::InvalidPackage("Missing field: Name".into()))?;
        let description = description
            .ok_or_else(|| ArkError::InvalidPackage("Missing field: Description".into()))?;
        let version =
            version.ok_or_else(|| ArkError::InvalidPackage("Missing field: Version".into()))?;
        let arch = arch.ok_or_else(|| ArkError::InvalidPackage("Missing field: Arch".into()))?;

        Ok(Self {
            name,
            description,
            version,
            arch,
            url,
            license,
            maintainer,
            dependencies,
            provides,
            conflicts,
        })
    }

    /// Serialize metadata back to plain text `ARKPKG` format.
    pub fn to_string_pretty(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("Name: {}\n", self.name));
        out.push_str(&format!("Description: {}\n", self.description));
        out.push_str(&format!("Version: {}\n", self.version));
        out.push_str(&format!("Arch: {}\n", self.arch));

        if let Some(url) = &self.url {
            out.push_str(&format!("URL: {}\n", url));
        }
        if let Some(license) = &self.license {
            out.push_str(&format!("License: {}\n", license));
        }
        if let Some(maintainer) = &self.maintainer {
            out.push_str(&format!("Maintainer: {}\n", maintainer));
        }

        if !self.dependencies.is_empty() {
            out.push_str("Dependencies:\n");
            for dep in &self.dependencies {
                out.push_str(&format!("{}\n", dep));
            }
        }

        if !self.provides.is_empty() {
            out.push_str("Provides:\n");
            for p in &self.provides {
                out.push_str(&format!("{}\n", p));
            }
        }

        if !self.conflicts.is_empty() {
            out.push_str("Conflicts:\n");
            for c in &self.conflicts {
                out.push_str(&format!("{}\n", c));
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_metadata() {
        let sample = r#"
Name: bash
Description: GNU Bourne Again Shell
Version: 5.3.0
Arch: x86_64
URL: https://www.gnu.org/software/bash/
License: GPL-3.0
Maintainer: arkanaOS Team
Dependencies:
glibc>=2.41
ncurses>=6.5
Provides:
bash=/usr/bin/bash
sh=/usr/bin/bash
Conflicts:
dash
busybox-sh
"#;

        let meta = Metadata::parse(sample).unwrap();
        assert_eq!(meta.name, "bash");
        assert_eq!(meta.version.as_str(), "5.3.0");
        assert_eq!(meta.arch, "x86_64");
        assert_eq!(meta.dependencies.len(), 2);
        assert_eq!(meta.provides.len(), 2);
        assert_eq!(meta.conflicts.len(), 2);
    }
}
