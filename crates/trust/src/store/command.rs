//! Running OS trust-store tools (`security`, `certutil`, `pkexec`).

use std::ffi::OsStr;
use std::io;
use std::process::{Command, Output};

use crate::TrustError;

/// A tool invocation: program name plus the hint shown when it is missing.
#[derive(Debug, Clone, Copy)]
pub(super) struct Tool {
    pub(super) program: &'static str,
    pub(super) missing_hint: &'static str,
}

impl Tool {
    /// Runs the tool to completion, capturing its output. A non-zero exit
    /// status is *not* an error here; callers inspect it (or, on Linux, use
    /// `run_checked`).
    pub(super) fn run<I, S>(self, args: I) -> Result<Output, TrustError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new(self.program)
            .args(args)
            .output()
            .map_err(|err| match err.kind() {
                io::ErrorKind::NotFound => TrustError::ToolMissing {
                    program: self.program,
                    hint: self.missing_hint,
                },
                _ => TrustError::io(self.program, err),
            })
    }

    /// Like [`Self::run`] but turns a non-zero exit status into
    /// [`TrustError::Command`]. Only the Linux store needs this: the macOS
    /// store maps `security` failures itself to detect a cancelled dialog.
    #[cfg(target_os = "linux")]
    pub(super) fn run_checked<I, S>(self, args: I) -> Result<Output, TrustError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.run(args)?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(self.failure(&output))
        }
    }

    /// Builds the error describing a failed run.
    pub(super) fn failure(self, output: &Output) -> TrustError {
        TrustError::Command {
            program: self.program,
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        }
    }
}
