use std::net::SocketAddr;

use crate::Mode;

/// Snapshot of the proxy state, published through a `tokio::sync::watch`
/// channel (see [`crate::ProxyHandle::subscribe`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyStatus {
    /// Current mode.
    pub mode: Mode,
    /// Whether capture-to-disk is enabled.
    pub capture: bool,
    /// Whether Apple Music was reachable at the last probe.
    pub online: bool,
    /// Whether the proxy server task is running.
    pub running: bool,
    /// Address the proxy is listening on.
    pub listening: SocketAddr,
    /// Traffic counters since start.
    pub counters: Counters,
    /// Most recent error, for display. Never contains token values.
    pub last_error: Option<String>,
}

/// Traffic counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    /// Requests received by the proxy (including CONNECT tunnels).
    pub requests_seen: u64,
    /// Requests answered locally by an interceptor instead of upstream.
    pub served_locally: u64,
}
