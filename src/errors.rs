use thiserror::Error;

/// Standard error type for arkpkg operations.
#[derive(Debug, Error)]
pub enum ArkError {
    #[error("Generic error: {0}")]
    Generic(String),

    #[error("Package not found: {0}")]
    PackageNotFound(String),

    #[error("Missing dependency: {0}\nPlease install the required package before continuing.")]
    MissingDependency(String),

    #[error(
        "Architecture mismatch: package architecture '{package_arch}' does not match system architecture '{system_arch}'"
    )]
    ArchitectureMismatch {
        package_arch: String,
        system_arch: String,
    },

    #[error("Verification failed: {0}")]
    VerificationFailed(String),

    #[error("Package already installed: {0}")]
    PackageAlreadyInstalled(String),

    #[error("Package conflict: {0}")]
    PackageConflict(String),

    #[error("Invalid package: {0}")]
    InvalidPackage(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("User cancelled operation")]
    UserCancelled,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl ArkError {
    /// Returns the standard exit code associated with this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            ArkError::Generic(_) => 1,
            ArkError::PackageNotFound(_) => 2,
            ArkError::MissingDependency(_) => 3,
            ArkError::ArchitectureMismatch { .. } => 4,
            ArkError::VerificationFailed(_) => 5,
            ArkError::PackageAlreadyInstalled(_) => 6,
            ArkError::PackageConflict(_) => 7,
            ArkError::InvalidPackage(_) => 8,
            ArkError::PermissionDenied(_) => 9,
            ArkError::UserCancelled => 10,
            ArkError::Io(err) => match err.kind() {
                std::io::ErrorKind::PermissionDenied => 9,
                std::io::ErrorKind::NotFound => 2,
                _ => 1,
            },
        }
    }
}

pub type Result<T> = std::result::Result<T, ArkError>;
