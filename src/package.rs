use std::path::PathBuf;

use crate::metadata::Metadata;
use crate::version::Version;

/// Represents a package being inspected or operated on.
#[derive(Debug, Clone)]
pub struct Package {
    pub metadata: Metadata,
    pub path: Option<PathBuf>,
}

impl Package {
    pub fn new(metadata: Metadata, path: Option<PathBuf>) -> Self {
        Self { metadata, path }
    }

    pub fn name(&self) -> &str {
        &self.metadata.name
    }

    pub fn version(&self) -> &Version {
        &self.metadata.version
    }

    pub fn arch(&self) -> &str {
        &self.metadata.arch
    }
}
