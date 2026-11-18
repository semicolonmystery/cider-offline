use serde::{Deserialize, Serialize};

/// How the proxy treats traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Forward every request untouched: no TLS interception, parsing,
    /// caching or token capture.
    Passthrough,
    /// Intercept Apple Music hosts and run them through the interceptor
    /// pipeline. Other hosts are still tunnelled untouched.
    #[default]
    Active,
}

impl Mode {
    /// Whether Apple Music traffic is intercepted in this mode.
    pub fn intercepts(self) -> bool {
        matches!(self, Self::Active)
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Passthrough => "passthrough",
            Self::Active => "active",
        })
    }
}
