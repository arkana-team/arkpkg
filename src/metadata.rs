use std::collections::HashSet;
use std::fmt;
use std::io::Read;

use crate::errors::{ArkError, Result};
use crate::version::{DependencyReq, Version};

/// Maximum uncompressed size of an archive's `ARKPKG` metadata (1 MiB).
pub(crate) const MAX_METADATA_SIZE: u64 = 1024 * 1024;

pub(crate) fn check_metadata_size(size: u64) -> Result<()> {
    if size > MAX_METADATA_SIZE {
        return Err(ArkError::InvalidPackage(format!(
            "ARKPKG metadata exceeds the {MAX_METADATA_SIZE} byte size limit"
        )));
    }
    Ok(())
}

/// Checks the declared size and bounds the read independently before parsing.
pub(crate) fn read_metadata(reader: impl Read, declared_size: u64) -> Result<Metadata> {
    check_metadata_size(declared_size)?;
    let mut content = String::new();
    reader
        .take(MAX_METADATA_SIZE + 1)
        .read_to_string(&mut content)?;
    check_metadata_size(content.len() as u64)?;
    if content.len() as u64 != declared_size {
        return Err(ArkError::InvalidPackage(
            "Incomplete ARKPKG metadata within the decoded search limit".into(),
        ));
    }
    Metadata::parse(&content)
}

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

fn extend_provides(value: &str, provides: &mut Vec<ProvideItem>) -> Result<()> {
    for item in value.split(',') {
        provides.push(ProvideItem::parse(item)?);
    }
    Ok(())
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

pub(crate) fn validate_package_atom(value: &str, field: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte));
    if valid {
        Ok(())
    } else {
        Err(ArkError::InvalidPackage(format!(
            "Invalid {field} '{value}': use ASCII letters, digits, '.', '_', '+', and '-'"
        )))
    }
}

pub(crate) fn validate_package_name(name: &str) -> Result<()> {
    validate_package_atom(name, "package name")
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
                        validate_package_atom(field_val, "architecture")?;
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
                            extend_provides(field_val, &mut provides)?;
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
                                "Provides" => extend_provides(trimmed, &mut provides)?,
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
                    "Provides" => extend_provides(trimmed, &mut provides)?,
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
        validate_package_name(&name)?;
        let description = description
            .ok_or_else(|| ArkError::InvalidPackage("Missing field: Description".into()))?;
        let version =
            version.ok_or_else(|| ArkError::InvalidPackage("Missing field: Version".into()))?;
        let arch = arch.ok_or_else(|| ArkError::InvalidPackage("Missing field: Arch".into()))?;
        for dependency in &dependencies {
            validate_package_name(&dependency.name)?;
        }
        for provide in &provides {
            validate_package_name(&provide.name)?;
        }
        for conflict in &conflicts {
            validate_package_name(conflict)?;
        }

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
    fn metadata_reader_rejects_oversized_declarations_without_reading() {
        let mut reader = std::io::Cursor::new(b"unread");
        assert!(matches!(
            read_metadata(&mut reader, u64::MAX),
            Err(ArkError::InvalidPackage(message)) if message.contains("size limit")
        ));
        assert_eq!(reader.position(), 0);
    }

    #[test]
    fn metadata_reader_bounds_reads_even_when_declared_size_is_small() {
        let mut reader = std::io::repeat(b'x').take(MAX_METADATA_SIZE * 2);
        assert!(matches!(
            read_metadata(&mut reader, 0),
            Err(ArkError::InvalidPackage(message)) if message.contains("size limit")
        ));
        assert_eq!(reader.limit(), MAX_METADATA_SIZE - 1);
    }

    #[test]
    fn metadata_reader_preserves_utf8_and_parse_errors() {
        assert!(matches!(
            read_metadata(&b"\xff"[..], 1),
            Err(ArkError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidData
        ));
        assert!(matches!(
            read_metadata(&b"Name: incomplete\n"[..], 17),
            Err(ArkError::InvalidPackage(message)) if message == "Missing field: Description"
        ));
    }

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

    #[test]
    fn rejects_package_names_that_could_escape_database_paths() {
        for name in ["../outside", "a/b", "", ".hidden", "name with spaces"] {
            let content = format!("Name: {name}\nDescription: test\nVersion: 1.0\nArch: any\n");
            assert!(Metadata::parse(&content).is_err(), "accepted {name:?}");
        }
    }

    #[test]
    fn rejects_unsafe_dependency_provide_and_conflict_names() {
        for field in ["Dependencies", "Provides", "Conflicts"] {
            let content = format!(
                "Name: safe\nDescription: test\nVersion: 1.0\nArch: any\n{field}:\n../outside\n"
            );
            assert!(
                Metadata::parse(&content).is_err(),
                "accepted unsafe {field}"
            );
        }
    }
}
