//! Periodic Apple Music reachability probe.

use std::time::Duration;

use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::config::ConnectivitySettings;

/// Errors creating a [`ConnectivityMonitor`].
#[derive(Debug, thiserror::Error)]
pub enum ConnectivityError {
    /// The HTTP client could not be built.
    #[error("could not build HTTP client: {0}")]
    Client(#[from] reqwest::Error),
}

/// Probes a URL on an interval and reports online/offline transitions.
#[derive(Debug, Clone)]
pub struct ConnectivityMonitor {
    client: reqwest::Client,
    probe_url: String,
    interval: Duration,
}

impl ConnectivityMonitor {
    /// Creates a monitor from settings.
    ///
    /// The client bypasses every proxy (including environment variables) so
    /// probes never loop back through this proxy.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectivityError::Client`] if the TLS backend fails to initialize.
    pub fn new(settings: &ConnectivitySettings) -> Result<Self, ConnectivityError> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(settings.timeout())
            .connect_timeout(settings.timeout())
            .build()?;
        Ok(Self {
            client,
            probe_url: settings.probe_url.clone(),
            interval: settings.interval(),
        })
    }

    /// Performs one probe. Any HTTP response (even an error status) means the
    /// host is reachable.
    pub async fn probe(&self) -> bool {
        match self.client.head(&self.probe_url).send().await {
            Ok(_) => true,
            Err(error) => {
                tracing::debug!(%error, "connectivity probe failed");
                false
            }
        }
    }

    /// Probes immediately, then on every interval, until `cancel` fires.
    /// `on_change` is called with the first result and on every transition.
    pub async fn run(self, cancel: CancellationToken, mut on_change: impl FnMut(bool) + Send) {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last = None;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = ticker.tick() => {}
            }
            let online = tokio::select! {
                () = cancel.cancelled() => break,
                online = self.probe() => online,
            };
            if last != Some(online) {
                tracing::info!(online, "connectivity changed");
                on_change(online);
                last = Some(online);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn settings(url: String) -> ConnectivitySettings {
        ConnectivitySettings {
            enabled: true,
            probe_url: url,
            interval_secs: 1,
            timeout_secs: 2,
            force_offline: false,
        }
    }

    #[tokio::test]
    async fn probe_should_report_offline_when_unreachable() {
        // Bind then drop to get a port with nothing listening.
        let port = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let monitor =
            ConnectivityMonitor::new(&settings(format!("http://127.0.0.1:{port}/"))).unwrap();
        assert!(!monitor.probe().await);
    }

    #[tokio::test]
    async fn probe_should_report_online_for_any_http_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let monitor = ConnectivityMonitor::new(&settings(format!("http://{addr}/"))).unwrap();
        assert!(monitor.probe().await);
    }
}
