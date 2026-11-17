//! The interceptor pipeline applied to Apple Music traffic in [`Mode::Active`].
//!
//! [`Mode::Active`]: crate::Mode::Active

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use hudsucker::Body;
use hudsucker::hyper::http::Extensions;
use hudsucker::hyper::{HeaderMap, Method, Request, Response, Uri};
use tokio_util::task::TaskTracker;

use crate::storage::Storage;

use super::cache_key::cache_key;

/// Result of an interceptor's request hook.
#[derive(Debug)]
pub enum RequestOutcome {
    /// Pass the (possibly modified) request to the next interceptor, and
    /// finally upstream.
    Continue(Request<Body>),
    /// Answer locally; upstream is not contacted and later interceptors'
    /// request hooks are skipped.
    Respond(Response<Body>),
}

/// A request/response hook for Apple Music traffic in [`crate::Mode::Active`].
///
/// Interceptors run in registration order for requests and in reverse order
/// for responses (middleware style). The response hook of an interceptor runs
/// only if its request hook ran, including when a later interceptor answered
/// locally (check [`Exchange::served_locally`]).
///
/// Implementations must never log token or cookie values.
#[async_trait]
pub trait Interceptor: Send + Sync + 'static {
    /// Short name for logs.
    fn name(&self) -> &'static str;

    /// Called with the request before it is forwarded.
    async fn on_request(&self, exchange: &mut Exchange, request: Request<Body>) -> RequestOutcome {
        let _ = exchange;
        RequestOutcome::Continue(request)
    }

    /// Called with the response (upstream or local) before it reaches the client.
    async fn on_response(
        &self,
        exchange: &mut Exchange,
        response: Response<Body>,
    ) -> Response<Body> {
        let _ = exchange;
        response
    }

    /// Called when forwarding upstream failed (e.g. offline). Returning a
    /// response answers the client with it; the first interceptor returning
    /// `Some` wins. Defaults to `None` (client gets `502 Bad Gateway`).
    async fn on_upstream_error(&self, exchange: &mut Exchange) -> Option<Response<Body>> {
        let _ = exchange;
        None
    }
}

/// Per-request state shared by all interceptor hooks of one request/response pair.
#[derive(Debug)]
pub struct Exchange {
    id: u64,
    method: Method,
    uri: Uri,
    request_headers: HeaderMap,
    online: bool,
    started: Instant,
    served_locally: bool,
    storage: Storage,
    tasks: TaskTracker,
    extensions: Extensions,
}

impl Exchange {
    pub(crate) fn new(
        id: u64,
        request: &Request<Body>,
        online: bool,
        storage: Storage,
        tasks: TaskTracker,
    ) -> Self {
        Self {
            id,
            method: request.method().clone(),
            uri: request.uri().clone(),
            request_headers: request.headers().clone(),
            online,
            started: Instant::now(),
            served_locally: false,
            storage,
            tasks,
            extensions: Extensions::new(),
        }
    }

    /// Monotonic id of this exchange (unique per proxy run).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Request method.
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Absolute request URI (`https://host/path?query`).
    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// Request host, lower-cased as received.
    pub fn host(&self) -> &str {
        self.uri.host().unwrap_or_default()
    }

    /// Original request headers (snapshot taken before any interceptor ran).
    /// May contain secrets: never log values.
    pub fn request_headers(&self) -> &HeaderMap {
        &self.request_headers
    }

    /// Normalized cache key of this request (see [`super::cache_key()`]).
    pub fn cache_key(&self) -> Option<String> {
        cache_key(&self.method, &self.uri)
    }

    /// Whether Apple Music was reachable when the request arrived.
    pub fn online(&self) -> bool {
        self.online
    }

    /// When the request arrived.
    pub fn started(&self) -> Instant {
        self.started
    }

    /// Whether the response was produced locally rather than by upstream.
    pub fn served_locally(&self) -> bool {
        self.served_locally
    }

    pub(crate) fn mark_served_locally(&mut self) {
        self.served_locally = true;
    }

    /// The local store.
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Spawns background work (e.g. a cache write) that should not delay the
    /// response. Tracked so proxy shutdown waits for it.
    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.tasks.spawn(future);
    }

    /// Typed scratch space for passing data from a request hook to a response hook.
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// Mutable typed scratch space.
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }
}

/// Ordered interceptor list.
#[derive(Clone, Default)]
pub(crate) struct Pipeline {
    interceptors: Arc<[Arc<dyn Interceptor>]>,
}

/// How far the request phase got; response hooks run for the first `ran` interceptors.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Progress {
    ran: usize,
}

impl Pipeline {
    pub(crate) fn new(interceptors: Vec<Arc<dyn Interceptor>>) -> Self {
        Self {
            interceptors: interceptors.into(),
        }
    }

    pub(crate) async fn run_request(
        &self,
        exchange: &mut Exchange,
        mut request: Request<Body>,
    ) -> (RequestOutcome, Progress) {
        for (index, interceptor) in self.interceptors.iter().enumerate() {
            match interceptor.on_request(exchange, request).await {
                RequestOutcome::Continue(next) => request = next,
                RequestOutcome::Respond(response) => {
                    tracing::debug!(
                        interceptor = interceptor.name(),
                        id = exchange.id(),
                        "answered locally"
                    );
                    exchange.mark_served_locally();
                    return (
                        RequestOutcome::Respond(response),
                        Progress { ran: index + 1 },
                    );
                }
            }
        }
        (
            RequestOutcome::Continue(request),
            Progress {
                ran: self.interceptors.len(),
            },
        )
    }

    pub(crate) async fn run_response(
        &self,
        exchange: &mut Exchange,
        progress: Progress,
        mut response: Response<Body>,
    ) -> Response<Body> {
        for interceptor in self.interceptors[..progress.ran].iter().rev() {
            response = interceptor.on_response(exchange, response).await;
        }
        response
    }

    pub(crate) async fn run_upstream_error(
        &self,
        exchange: &mut Exchange,
        progress: Progress,
    ) -> Option<Response<Body>> {
        for interceptor in &self.interceptors[..progress.ran] {
            if let Some(response) = interceptor.on_upstream_error(exchange).await {
                exchange.mark_served_locally();
                return Some(response);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records hook order into a shared log; optionally answers locally.
    struct Recorder {
        name: &'static str,
        log: Arc<Mutex<Vec<String>>>,
        respond: bool,
    }

    #[async_trait]
    impl Interceptor for Recorder {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn on_request(&self, _: &mut Exchange, request: Request<Body>) -> RequestOutcome {
            self.log.lock().unwrap().push(format!("req:{}", self.name));
            if self.respond {
                RequestOutcome::Respond(Response::new(Body::empty()))
            } else {
                RequestOutcome::Continue(request)
            }
        }

        async fn on_response(&self, _: &mut Exchange, response: Response<Body>) -> Response<Body> {
            self.log.lock().unwrap().push(format!("res:{}", self.name));
            response
        }
    }

    fn exchange() -> (Exchange, Request<Body>) {
        let request = Request::builder()
            .uri("https://amp-api.music.apple.com/v1/test")
            .body(Body::empty())
            .unwrap();
        let exchange = Exchange::new(
            1,
            &request,
            true,
            Storage::open_in_memory().unwrap(),
            TaskTracker::new(),
        );
        (exchange, request)
    }

    fn pipeline(log: &Arc<Mutex<Vec<String>>>, responder: Option<&'static str>) -> Pipeline {
        let make = |name| -> Arc<dyn Interceptor> {
            Arc::new(Recorder {
                name,
                log: Arc::clone(log),
                respond: responder == Some(name),
            })
        };
        Pipeline::new(vec![make("a"), make("b"), make("c")])
    }

    #[tokio::test]
    async fn hooks_should_run_in_middleware_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let pipeline = pipeline(&log, None);
        let (mut ex, req) = exchange();
        let (_, progress) = pipeline.run_request(&mut ex, req).await;
        pipeline
            .run_response(&mut ex, progress, Response::new(Body::empty()))
            .await;
        assert_eq!(
            *log.lock().unwrap(),
            ["req:a", "req:b", "req:c", "res:c", "res:b", "res:a"]
        );
    }

    #[tokio::test]
    async fn local_response_should_skip_later_interceptors() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let pipeline = pipeline(&log, Some("b"));
        let (mut ex, req) = exchange();
        let (outcome, progress) = pipeline.run_request(&mut ex, req).await;
        let RequestOutcome::Respond(res) = outcome else {
            panic!("expected a local response");
        };
        pipeline.run_response(&mut ex, progress, res).await;
        assert_eq!(*log.lock().unwrap(), ["req:a", "req:b", "res:b", "res:a"]);
    }

    #[tokio::test]
    async fn local_response_should_mark_exchange() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let pipeline = pipeline(&log, Some("a"));
        let (mut ex, req) = exchange();
        pipeline.run_request(&mut ex, req).await;
        assert!(ex.served_locally());
    }
}
