use std::path::PathBuf;

/// Errors produced by CA management and trust-store operations.
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    /// Reading or writing a CA file failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// File or directory that was being accessed.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Generating, parsing or serializing a certificate or key failed.
    #[error("certificate error: {0}")]
    Certificate(#[from] rcgen::Error),
    /// The stored certificate file is not valid PEM.
    #[error("invalid PEM certificate: {0}")]
    Pem(#[from] rustls_pki_types::pem::Error),
    /// Only one of the certificate / key files exists. The CA is not silently
    /// regenerated, since that would break an already trusted installation.
    #[error("incomplete CA in {dir}: certificate and key must both exist")]
    Incomplete {
        /// CA directory.
        dir: PathBuf,
    },
    /// The requested operation has no implementation on this platform.
    #[error("{operation} is not implemented on this platform")]
    NotImplemented {
        /// Name of the unimplemented operation.
        operation: &'static str,
    },
    /// The user dismissed the OS consent / elevation prompt.
    #[error("the operation was cancelled by the user")]
    Cancelled,
    /// A required OS tool is not installed.
    #[error("required tool `{program}` was not found; {hint}")]
    ToolMissing {
        /// Executable that could not be started.
        program: &'static str,
        /// Human-readable installation hint.
        hint: &'static str,
    },
    /// An OS tool ran but reported failure.
    #[error("`{program}` failed ({status}): {stderr}")]
    Command {
        /// Executable that failed.
        program: &'static str,
        /// Exit status description.
        status: String,
        /// Trimmed standard error output of the tool.
        stderr: String,
    },
    /// A native certificate store API call failed.
    #[error("certificate store error: {0}")]
    Store(#[source] std::io::Error),
}

impl TrustError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
