//! The `hudsucker` HTTP handler: mode gating, Apple host routing, pipeline
//! dispatch, capture and counters.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hudsucker::hyper::{Method, Request, Response, StatusCode};
use hudsucker::{Body, HttpContext, HttpHandler, RequestOrResponse};
use tokio::sync::watch;
use tokio_util::task::TaskTracker;

use super::capture::{self, CaptureWriter, PendingCapture};
use super::hosts::{is_apple_music_host, is_offline_only_host};
use super::interceptor::{Exchange, Pipeline, Progress, RequestOutcome};
use super::status::ProxyStatus;
use crate::Mode;
use crate::storage::Storage;

/// State shared by all handler clones of one proxy instance.
pub(crate) struct Shared {
    pub(crate) status: watch::Sender<ProxyStatus>,
    pub(crate) pipeline: Pipeline,
    pub(crate) storage: Storage,
    pub(crate) captures: CaptureWriter,
    pub(crate) tasks: TaskTracker,
    next_id: AtomicU64,
}

impl Shared {
    pub(crate) fn new(
        status: watch::Sender<ProxyStatus>,
        pipeline: Pipeline,
        storage: Storage,
        captures: CaptureWriter,
        tasks: TaskTracker,
    ) -> Self {
        Self {
            status,
            pipeline,
            storage,
            captures,
            tasks,
            next_id: AtomicU64::new(1),
        }
    }
}

/// Whether a request to `host` is intercepted (MITM + pipeline) in `mode`.
/// Offline, the [offline-only hosts](super::hosts::OFFLINE_ONLY_DOMAIN) are
/// intercepted too, so the offline responder can answer them.
pub(crate) fn should_intercept_host(mode: Mode, online: bool, host: &str) -> bool {
    mode.intercepts() && (is_apple_music_host(host) || (!online && is_offline_only_host(host)))
}

/// A request that went through the pipeline and awaits its response.
struct InFlight {
    exchange: Exchange,
    progress: Progress,
    capture: Option<PendingCapture>,
}

/// `hudsucker` clones the handler per request; the clone carries that
/// request's [`InFlight`] state from `handle_request` to `handle_response`.
pub(crate) struct ProxyHttpHandler {
    shared: Arc<Shared>,
    in_flight: Option<InFlight>,
}

impl Clone for ProxyHttpHandler {
    /// Clones never inherit in-flight state: each request starts fresh.
    fn clone(&self) -> Self {
        Self::new(Arc::clone(&self.shared))
    }
}

impl ProxyHttpHandler {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            in_flight: None,
        }
    }

    /// Request-phase logic, independent of `hudsucker`'s context type.
    pub(crate) async fn process_request(&mut self, request: Request<Body>) -> RequestOrResponse {
        let mut snapshot = None;
        self.shared.status.send_modify(|status| {
            status.counters.requests_seen += 1;
            snapshot = Some((status.mode, status.capture, status.online));
        });
        let Some((mode, capture_enabled, online)) = snapshot else {
            return request.into();
        };

        // Passthrough, CONNECT (decided in `should_intercept`) and non-Apple
        // hosts are forwarded without any processing.
        if request.method() == Method::CONNECT
            || !request
                .uri()
                .host()
                .is_some_and(|host| should_intercept_host(mode, online, host))
        {
            return request.into();
        }

        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let mut exchange = Exchange::new(
            id,
            &request,
            online,
            self.shared.storage.clone(),
            self.shared.tasks.clone(),
        );

        let (request, capture) = if capture_enabled {
            let (request, captured) = capture::capture_request(request).await;
            (request, Some(PendingCapture { request: captured }))
        } else {
            (request, None)
        };

        let pipeline = self.shared.pipeline.clone();
        let (outcome, progress) = pipeline.run_request(&mut exchange, request).await;
        let mut in_flight = InFlight {
            exchange,
            progress,
            capture,
        };
        match outcome {
            RequestOutcome::Continue(request) => {
                self.in_flight = Some(in_flight);
                request.into()
            }
            RequestOutcome::Respond(response) => {
                self.count_served_locally();
                self.finish(&mut in_flight, response).await.into()
            }
        }
    }

    /// Runs response hooks and capture for an in-flight exchange.
    async fn finish(&self, in_flight: &mut InFlight, response: Response<Body>) -> Response<Body> {
        let was_local = in_flight.exchange.served_locally();
        let response = self
            .shared
            .pipeline
            .run_response(&mut in_flight.exchange, in_flight.progress, response)
            .await;
        // A response hook replaced an upstream answer with a local one.
        if !was_local && in_flight.exchange.served_locally() {
            self.count_served_locally();
        }
        match &in_flight.capture {
            Some(pending) => {
                let (response, captured) = capture::capture_response(response).await;
                self.shared
                    .captures
                    .write(&in_flight.exchange, pending, Some(&captured));
                response
            }
            None => response,
        }
    }

    fn count_served_locally(&self) {
        self.shared
            .status
            .send_modify(|status| status.counters.served_locally += 1);
    }

    fn record_error(&self, message: String) {
        self.shared
            .status
            .send_modify(|status| status.last_error = Some(message));
    }
}

impl HttpHandler for ProxyHttpHandler {
    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        request: Request<Body>,
    ) -> RequestOrResponse {
        self.process_request(request).await
    }

    async fn handle_response(
        &mut self,
        _ctx: &HttpContext,
        response: Response<Body>,
    ) -> Response<Body> {
        match self.in_flight.take() {
            Some(mut in_flight) => self.finish(&mut in_flight, response).await,
            None => response,
        }
    }

    async fn handle_error(
        &mut self,
        _ctx: &HttpContext,
        error: hudsucker::hyper_util::client::legacy::Error,
    ) -> Response<Body> {
        let Some(mut in_flight) = self.in_flight.take() else {
            self.record_error(format!("upstream request failed: {error}"));
            return bad_gateway();
        };
        let host = in_flight.exchange.host().to_owned();
        tracing::debug!(%host, %error, "upstream request failed");
        let pipeline = self.shared.pipeline.clone();
        if let Some(response) = pipeline
            .run_upstream_error(&mut in_flight.exchange, in_flight.progress)
            .await
        {
            self.count_served_locally();
            return self.finish(&mut in_flight, response).await;
        }
        self.record_error(format!("upstream request to {host} failed: {error}"));
        if let Some(pending) = &in_flight.capture {
            self.shared
                .captures
                .write(&in_flight.exchange, pending, None);
        }
        bad_gateway()
    }

    async fn should_intercept(&mut self, _ctx: &HttpContext, request: &Request<Body>) -> bool {
        let (mode, online) = {
            let status = self.shared.status.borrow();
            (status.mode, status.online)
        };
        request
            .uri()
            .host()
            .is_some_and(|host| should_intercept_host(mode, online, host))
    }
}

fn bad_gateway() -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::BAD_GATEWAY;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_mode_should_intercept_apple_hosts() {
        assert!(should_intercept_host(
            Mode::Active,
            true,
            "amp-api.music.apple.com"
        ));
    }

    #[test]
    fn blobstore_should_be_intercepted_only_offline() {
        let host = "store-035.blobstore.apple.com";
        assert_eq!(
            (
                should_intercept_host(Mode::Active, true, host),
                should_intercept_host(Mode::Active, false, host)
            ),
            (false, true)
        );
    }

    #[test]
    fn active_mode_should_not_intercept_other_hosts() {
        assert!(!should_intercept_host(Mode::Active, false, "example.com"));
    }

    #[test]
    fn passthrough_mode_should_never_intercept() {
        assert!(!should_intercept_host(
            Mode::Passthrough,
            false,
            "amp-api.music.apple.com"
        ));
    }
}
