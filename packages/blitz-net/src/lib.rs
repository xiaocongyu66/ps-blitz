//! Networking (HTTP, filesystem, Data URIs) for Blitz
//!
//! Provides implementations of [`blitz_traits::net::NetProvider`], which loads
//! the resources a document needs, and of
//! [`blitz_traits::platform::FetchProvider`], which serves a guest's `fetch()`.
//!
//! **Both run on the one client.** `Provider` holds a single `reqwest::Client`,
//! built once with HTTP/2, the cookie jar, the compression codecs and the
//! cacache disk cache, plus one per-host semaphore enforcing the browser's cap
//! of six concurrent requests per origin. A guest's `fetch` therefore shares
//! the connection pool, the cookies and the cache with the page's own loads,
//! and counts against the same concurrency budget, which is what a browser
//! does. A second client would have given a guest its own cookie jar and its
//! own six connections per host.

use blitz_traits::net::{AbortSignal, Body, Bytes, NetHandler, NetProvider, NetWaker, Request};
use blitz_traits::platform::{
    FetchError, FetchHandler, FetchProvider, FetchRequest, FetchResponse, HeaderMap, StatusCode,
};
use data_url::DataUrl;
use std::{
    collections::HashMap,
    marker::PhantomData,
    pin::Pin,
    sync::{Arc, Mutex},
    task::Poll,
};
use tokio::sync::Semaphore;

/// Cookie storage supplied to [`Provider::with_user_agent_and_cookie_provider`].
///
/// Re-exported so an embedder can provide a persistent profile jar without
/// depending on reqwest directly. Reqwest invokes it for every response in a
/// redirect chain and before every request it sends.
#[cfg(feature = "cookies")]
pub use reqwest::cookie::CookieStore;

#[cfg(feature = "cache")]
use http_cache_reqwest::{
    CACacheManager, Cache, CacheMode, CacheOptions, HttpCache, HttpCacheOptions,
};

/// The `User-Agent` sent when a caller does not choose one.
///
/// Kept as it was so that existing consumers see no change in what servers
/// send them. It is worth knowing that this string is a 2020 Firefox on Linux,
/// and that a large share of the popular web serves a degraded page, a
/// challenge stub or nothing at all to a client it does not recognise. A
/// consumer that wants the page a current browser would get has to say so with
/// [`Provider::with_user_agent`].
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:60.0) Gecko/20100101 Firefox/81.0";

/// Matches real browsers' per-origin cap of 6.
const PER_HOST_MAX_CONCURRENT: usize = 6;

type HostLimits = Arc<Mutex<HashMap<String, Arc<Semaphore>>>>;

#[cfg(feature = "cache")]
type Client = reqwest_middleware::ClientWithMiddleware;
#[cfg(not(feature = "cache"))]
type Client = reqwest::Client;

#[cfg(feature = "cache")]
type RequestBuilder = reqwest_middleware::RequestBuilder;
#[cfg(not(feature = "cache"))]
type RequestBuilder = reqwest::RequestBuilder;

#[cfg(feature = "cache")]
fn get_cache_path() -> std::path::PathBuf {
    use directories::ProjectDirs;
    let path = ProjectDirs::from("com", "DioxusLabs", "Blitz")
        .expect("Failed to find cache directory")
        .cache_dir()
        .to_owned();
    #[cfg(feature = "tracing")]
    tracing::info!(path = ?path.display(), "Using cache dir");
    path
}

#[cfg(target_arch = "wasm32")]
fn spawn(fut: impl Future + 'static) {
    wasm_bindgen_futures::spawn_local(async move {
        fut.await;
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn<F>(fut: F)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(fut);
}

pub struct Provider {
    client: Client,
    waker: Arc<dyn NetWaker>,
    per_host_limits: HostLimits,
    user_agent: Arc<str>,
    #[cfg(feature = "cache")]
    cache_manager: CACacheManager,
}
impl Provider {
    pub fn new(waker: Option<Arc<dyn NetWaker>>) -> Self {
        Self::with_user_agent(waker, DEFAULT_USER_AGENT)
    }

    /// A provider that identifies itself as `user_agent`.
    ///
    /// The string is sent verbatim on document, subresource and script
    /// requests alike, because a server that decides what to serve from the
    /// `User-Agent` will do so on every one of them, and a client that agreed
    /// with itself on only some would fetch a page assembled for two different
    /// browsers.
    pub fn with_user_agent(waker: Option<Arc<dyn NetWaker>>, user_agent: &str) -> Self {
        let builder = reqwest::Client::builder();
        #[cfg(feature = "cookies")]
        let builder = builder.cookie_store(true);
        Self::with_client_builder(waker, user_agent, builder)
    }

    /// A provider that uses an embedder-owned cookie jar and identity.
    ///
    /// The jar is installed on the provider's one reqwest client, so it sees
    /// intermediate redirects and error responses as well as the final
    /// successful response. It also supplies cookies for redirected requests;
    /// no parallel client or second cookie store is involved.
    #[cfg(feature = "cookies")]
    pub fn with_user_agent_and_cookie_provider<C>(
        waker: Option<Arc<dyn NetWaker>>,
        user_agent: &str,
        cookie_provider: Arc<C>,
    ) -> Self
    where
        C: CookieStore + 'static,
    {
        let builder = reqwest::Client::builder().cookie_provider(cookie_provider);
        Self::with_client_builder(waker, user_agent, builder)
    }

    fn with_client_builder(
        waker: Option<Arc<dyn NetWaker>>,
        user_agent: &str,
        builder: reqwest::ClientBuilder,
    ) -> Self {
        let client = builder.build().unwrap();

        #[cfg(feature = "cache")]
        let cache_manager = CACacheManager::new(get_cache_path(), true);

        #[cfg(feature = "cache")]
        let client = reqwest_middleware::ClientBuilder::new(client)
            .with(Cache(HttpCache {
                mode: CacheMode::Default,
                manager: cache_manager.clone(),
                options: HttpCacheOptions {
                    // Evaluate cache policy as a single-user (private) cache, like a
                    // real browser, rather than a shared/proxy cache. The default
                    // (`shared: true`) treats any response carrying `Set-Cookie` without
                    // an explicit `Cache-Control: public`/`immutable` as immediately
                    // stale, forcing a revalidation request to the server on every load.
                    // Many CDNs (e.g. Wikimedia image hosts) serve images this way, so
                    // the shared-cache default defeats disk caching and gets us rate
                    // limited. A private cache honours heuristic freshness instead.
                    cache_options: Some(CacheOptions {
                        shared: false,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            }))
            .build();

        let waker = waker.unwrap_or(Arc::new(DummyNetWaker));
        Self {
            client,
            waker,
            per_host_limits: Arc::new(Mutex::new(HashMap::new())),
            user_agent: Arc::from(user_agent),
            #[cfg(feature = "cache")]
            cache_manager,
        }
    }
    pub fn shared(waker: Option<Arc<dyn NetWaker>>) -> Arc<dyn NetProvider> {
        Arc::new(Self::new(waker))
    }
    pub fn shared_with_user_agent(
        waker: Option<Arc<dyn NetWaker>>,
        user_agent: &str,
    ) -> Arc<dyn NetProvider> {
        Arc::new(Self::with_user_agent(waker, user_agent))
    }
    /// What this provider identifies itself as.
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }
    pub fn is_empty(&self) -> bool {
        Arc::strong_count(&self.waker) == 1
    }
    pub fn count(&self) -> usize {
        Arc::strong_count(&self.waker) - 1
    }

    #[cfg(feature = "cache")]
    pub async fn clear_cache(&self) {
        if let Err(e) = self.cache_manager.clear().await {
            #[cfg(feature = "tracing")]
            tracing::error!("Failed to clear HTTP cache: {:?}", e);
            #[cfg(not(feature = "tracing"))]
            let _ = e;
        }
    }
}
impl Provider {
    async fn fetch_inner(
        client: Client,
        request: Request,
        per_host_limits: HostLimits,
        user_agent: Arc<str>,
    ) -> Result<(String, Bytes), ProviderError> {
        match request.url.scheme() {
            "data" => {
                let data_url = DataUrl::process(request.url.as_str())?;
                let decoded = data_url.decode_to_vec()?;
                Ok((request.url.to_string(), Bytes::from(decoded.0)))
            }
            "file" => {
                let file_path = request
                    .url
                    .to_file_path()
                    .map_err(|()| std::io::Error::new(std::io::ErrorKind::InvalidInput, "file URL is not local"))?;
                let file_content = std::fs::read(file_path)?;
                Ok((request.url.to_string(), Bytes::from(file_content)))
            }
            _ => Self::fetch_http(client, request, per_host_limits, user_agent).await,
        }
    }

    async fn fetch_http(
        client: Client,
        request: Request,
        per_host_limits: HostLimits,
        user_agent: Arc<str>,
    ) -> Result<(String, Bytes), ProviderError> {
        // Acquire a per-host permit, held for the duration of the request, to
        // keep total in-flight requests per origin bounded.
        let host_key = request
            .url
            .host_str()
            .map(str::to_owned)
            .unwrap_or_default();
        let semaphore = {
            let mut map = per_host_limits.lock().unwrap();
            map.entry(host_key)
                .or_insert_with(|| Arc::new(Semaphore::new(PER_HOST_MAX_CONCURRENT)))
                .clone()
        };
        let _permit = semaphore
            .acquire()
            .await
            .expect("per-host semaphore was closed");

        let mut req = client
            .request(request.method, request.url)
            .headers(request.headers)
            .header("User-Agent", &*user_agent);

        if let Some(content_type) = request.content_type.as_ref() {
            req = req.header("Content-Type", content_type);
        }

        let req = req
            .apply_body(request.body, request.content_type.as_deref())
            .await;
        let response = req.send().await?;
        let status = response.status();
        let final_url = response.url().to_string();

        if status.is_success() {
            return Ok((final_url, response.bytes().await?));
        }

        #[cfg(feature = "tracing")]
        tracing::warn!(
            url = final_url.as_str(),
            status = status.as_u16(),
            "HTTP error status"
        );
        Err(ProviderError::HttpStatus {
            status,
            url: final_url,
        })
    }

    #[allow(clippy::type_complexity)]
    pub fn fetch_with_callback(
        &self,
        request: Request,
        callback: Box<dyn FnOnce(Result<(String, Bytes), ProviderError>) + Send + Sync + 'static>,
    ) {
        #[cfg(feature = "tracing")]
        let url = request.url.to_string();

        let client = self.client.clone();
        let per_host_limits = self.per_host_limits.clone();
        let user_agent = self.user_agent.clone();
        spawn(async move {
            let result = Self::fetch_inner(client, request, per_host_limits, user_agent).await;

            #[cfg(feature = "tracing")]
            if let Err(e) = &result {
                #[cfg(feature = "tracing")]
                tracing::error!(url = url.as_str(), error = ?e, "Fetching");
            } else {
                #[cfg(feature = "tracing")]
                tracing::info!(url = url.as_str(), "Success fetching");
            }

            callback(result);
        });
    }

    pub async fn fetch_async(&self, request: Request) -> Result<(String, Bytes), ProviderError> {
        #[cfg(feature = "tracing")]
        let url = request.url.to_string();

        let client = self.client.clone();
        let per_host_limits = self.per_host_limits.clone();
        let user_agent = self.user_agent.clone();
        let result = Self::fetch_inner(client, request, per_host_limits, user_agent).await;

        #[cfg(feature = "tracing")]
        if let Err(e) = &result {
            #[cfg(feature = "tracing")]
            tracing::error!(url = url.as_str(), error = ?e, "Fetching");
        } else {
            #[cfg(feature = "tracing")]
            tracing::info!(url = url.as_str(), "Success fetching");
        }

        result
    }

    /// Fetch, keeping the response metadata that [`Provider::fetch_async`]
    /// discards.
    ///
    /// `fetch_async` returns `(String, Bytes)`: the final URL and the body. That
    /// is the right shape for the overwhelmingly common case, a document or a
    /// subresource whose bytes are the whole answer, and it stays as it is.
    ///
    /// What it cannot answer is what the server *said* the bytes were. An
    /// embedder loading a WebAssembly module wants to reject
    /// `Content-Type: text/html` before handing the bytes to a parser that will
    /// report an offset into a file that is not a module at all. That check
    /// needs headers, and the headers already exist: `fetch_http` reads them off
    /// the response and drops them on the way out.
    ///
    /// Additive rather than a widening of `fetch_async`, deliberately. Changing
    /// that return type touches every caller in every embedder for a need only
    /// some of them have, and `HeaderMap` is a heap-allocated multimap the hot
    /// path would then build and clone for every subresource. This allocates
    /// only for the callers that ask.
    ///
    /// `data:` and `file:` URLs synthesise a response. A `data:` URL states its
    /// own mime type, so that becomes a real `Content-Type`; a `file:` URL has
    /// none, and a caller that requires one should read an absent header as
    /// unknown rather than as a mismatch.
    pub async fn fetch_response_async(
        &self,
        request: Request,
    ) -> Result<FetchResponse, ProviderError> {
        let url = request.url.clone();
        match url.scheme() {
            "data" => {
                // Scoped so the borrow of `url` ends before it is moved into
                // the response.
                let (body, headers) = {
                    let data_url = DataUrl::process(url.as_str())?;
                    let decoded = data_url.decode_to_vec()?;
                    let mut headers = HeaderMap::new();
                    if let Ok(value) = data_url.mime_type().to_string().parse() {
                        headers.insert(blitz_traits::platform::http::header::CONTENT_TYPE, value);
                    }
                    (Bytes::from(decoded.0), headers)
                };
                Ok(FetchResponse::new(url, StatusCode::OK)
                    .headers(headers)
                    .body(body))
            }
            "file" => {
                let file_path = url.to_file_path().map_err(|()| {
                    ProviderError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "file URL is not local",
                    ))
                })?;
                let file_content = std::fs::read(file_path)?;
                Ok(FetchResponse::new(url, StatusCode::OK).body(Bytes::from(file_content)))
            }
            _ => {
                let client = self.client.clone();
                let per_host_limits = self.per_host_limits.clone();
                let user_agent = self.user_agent.clone();
                Self::fetch_http_response(client, request, per_host_limits, user_agent).await
            }
        }
    }

    /// The HTTP half of [`Provider::fetch_response_async`].
    ///
    /// Deliberately a sibling of [`Provider::fetch_http`] rather than a wrapper
    /// around it: that one consumes the response to get at the body and cannot
    /// hand back what it read on the way. The per-host permit, the user agent
    /// and the non-2xx handling are the same, so the two must be changed
    /// together.
    async fn fetch_http_response(
        client: Client,
        request: Request,
        per_host_limits: HostLimits,
        user_agent: Arc<str>,
    ) -> Result<FetchResponse, ProviderError> {
        let host_key = request
            .url
            .host_str()
            .map(str::to_owned)
            .unwrap_or_default();
        let semaphore = {
            let mut map = per_host_limits.lock().unwrap();
            map.entry(host_key)
                .or_insert_with(|| Arc::new(Semaphore::new(PER_HOST_MAX_CONCURRENT)))
                .clone()
        };
        let _permit = semaphore
            .acquire()
            .await
            .expect("per-host semaphore was closed");

        let mut req = client
            .request(request.method, request.url)
            .headers(request.headers)
            .header("User-Agent", &*user_agent);

        if let Some(content_type) = request.content_type.as_ref() {
            req = req.header("Content-Type", content_type);
        }

        let req = req
            .apply_body(request.body, request.content_type.as_deref())
            .await;
        let response = req.send().await?;
        let status = response.status();
        let final_url = response.url().clone();

        if !status.is_success() {
            #[cfg(feature = "tracing")]
            tracing::warn!(
                url = final_url.as_str(),
                status = status.as_u16(),
                "HTTP error status"
            );
            return Err(ProviderError::HttpStatus {
                status,
                url: final_url.to_string(),
            });
        }

        // Read before the body, because taking the body consumes the response.
        let headers = response.headers().clone();
        Ok(FetchResponse::new(final_url, status)
            .headers(headers)
            .body(response.bytes().await?))
    }
}

/// The `fetch()` path.
///
/// Separate from [`Provider::fetch_inner`] rather than layered on it, and the
/// reason is the whole point of the trait: `fetch_inner` returns
/// `(String, Bytes)` and turns any non-success status into a `ProviderError`
/// that [`NetProvider::fetch`] then logs and drops. A caller loading an image
/// wants exactly that. A `fetch()` caller wants the 404.
impl Provider {
    async fn platform_fetch_inner(
        client: Client,
        request: FetchRequest,
        per_host_limits: HostLimits,
        user_agent: Arc<str>,
    ) -> Result<FetchResponse, FetchError> {
        match request.url.scheme() {
            "data" => Self::platform_fetch_data(request),
            "file" => Self::platform_fetch_file(request),
            "http" | "https" => {
                Self::platform_fetch_http(client, request, per_host_limits, user_agent).await
            }
            scheme => Err(FetchError::UnsupportedScheme(scheme.to_owned())),
        }
    }

    /// A `data:` URL, answered as a synthetic 200.
    ///
    /// The status and the `Content-Type` are invented, because a data URL has
    /// no server to supply them, and inventing them is what the fetch
    /// specification requires: a `data:` response is a 200 whose content type
    /// is the one encoded in the URL.
    fn platform_fetch_data(request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let data_url = DataUrl::process(request.url.as_str())
            .map_err(|err| FetchError::InvalidRequest(format!("{err:?}")))?;
        let mime = data_url.mime_type().to_string();
        let (body, _) = data_url
            .decode_to_vec()
            .map_err(|err| FetchError::InvalidRequest(format!("{err:?}")))?;

        let mut headers = HeaderMap::new();
        if let Ok(value) = mime.parse() {
            // Via `reqwest`, which re-exports `http`'s header types. Naming
            // `http` directly would mean a new dependency for one constant.
            headers.insert(reqwest::header::CONTENT_TYPE, value);
        }

        Ok(FetchResponse::new(request.url, StatusCode::OK)
            .headers(headers)
            .body(Bytes::from(body)))
    }

    /// A `file:` URL, answered as a synthetic 200.
    ///
    /// Goes through [`Url::to_file_path`] rather than `Url::path`, which is
    /// what [`Provider::fetch_inner`] uses. `path` hands back the URL's
    /// percent-encoded path component as a string, so a file whose name
    /// contains a space or a `#` is looked up under the wrong name, and on
    /// Windows the leading slash makes it wrong outright. `to_file_path`
    /// decodes and refuses a URL that does not name a local path.
    ///
    /// **This is not access control, and it is not claiming to be.** Whether a
    /// document may read local files at all is an origin question, and origins
    /// are not visible here; see `blitz-platform-api`, which holds the origin
    /// and is where such a policy belongs.
    fn platform_fetch_file(request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let path = request.url.to_file_path().map_err(|()| {
            FetchError::InvalidRequest(format!("not a local path: {}", request.url))
        })?;

        let body = std::fs::read(path).map_err(|err| FetchError::Network(err.to_string()))?;

        Ok(FetchResponse::new(request.url, StatusCode::OK).body(Bytes::from(body)))
    }

    async fn platform_fetch_http(
        client: Client,
        request: FetchRequest,
        per_host_limits: HostLimits,
        user_agent: Arc<str>,
    ) -> Result<FetchResponse, FetchError> {
        // The same per-origin permit the page's own loads take, so a guest
        // cannot open more connections to a host than a browser would.
        let host_key = request
            .url
            .host_str()
            .map(str::to_owned)
            .unwrap_or_default();
        let semaphore = {
            let mut map = per_host_limits.lock().unwrap();
            map.entry(host_key)
                .or_insert_with(|| Arc::new(Semaphore::new(PER_HOST_MAX_CONCURRENT)))
                .clone()
        };
        let _permit = semaphore
            .acquire()
            .await
            .expect("per-host semaphore was closed");

        let mut req = client
            .request(request.method, request.url)
            .headers(request.headers)
            .header("User-Agent", &*user_agent);

        if let Some(body) = request.body {
            req = req.body(body);
        }

        let response = req
            .send()
            .await
            .map_err(|err| FetchError::Network(err.to_string()))?;

        // Everything below here is what `fetch_inner` throws away.
        let status = response.status();
        let headers = response.headers().clone();
        let url = response.url().clone();
        let body = response
            .bytes()
            .await
            .map_err(|err| FetchError::Network(err.to_string()))?;

        Ok(FetchResponse::new(url, status).headers(headers).body(body))
    }
}

impl FetchProvider for Provider {
    fn fetch(&self, request: FetchRequest, handler: Box<dyn FetchHandler>) {
        let client = self.client.clone();
        let per_host_limits = self.per_host_limits.clone();
        let user_agent = self.user_agent.clone();

        #[cfg(feature = "tracing")]
        let url = request.url.to_string();

        spawn(async move {
            let result =
                Self::platform_fetch_inner(client, request, per_host_limits, user_agent).await;

            #[cfg(feature = "tracing")]
            match &result {
                Ok(response) => tracing::info!(
                    url = url.as_str(),
                    status = response.status.as_u16(),
                    "fetch complete"
                ),
                Err(error) => tracing::error!(url = url.as_str(), error = ?error, "fetch failed"),
            }

            handler.complete(result);
        });
    }
}

impl NetProvider for Provider {
    fn fetch(&self, doc_id: usize, mut request: Request, handler: Box<dyn NetHandler>) {
        let client = self.client.clone();
        let per_host_limits = self.per_host_limits.clone();
        let user_agent = self.user_agent.clone();

        #[cfg(feature = "tracing")]
        tracing::info!(url = request.url.as_str(), "Fetching");

        let waker = self.waker.clone();
        spawn(async move {
            #[cfg(feature = "tracing")]
            let url = request.url.to_string();

            let signal = request.signal.take();
            let result = if let Some(signal) = signal {
                AbortFetch::new(
                    signal,
                    Box::pin(async move {
                        Self::fetch_inner(client, request, per_host_limits, user_agent).await
                    }),
                )
                .await
            } else {
                Self::fetch_inner(client, request, per_host_limits, user_agent).await
            };

            waker.wake(doc_id);

            match result {
                Ok((response_url, bytes)) => {
                    handler.bytes(response_url, bytes);
                    #[cfg(feature = "tracing")]
                    tracing::info!(url = url.as_str(), "Success fetching");
                }
                Err(e) => {
                    #[cfg(feature = "tracing")]
                    tracing::error!(url = url.as_str(), error = ?e, "Error fetching");
                    #[cfg(not(feature = "tracing"))]
                    let _ = e;
                }
            };
        });
    }
}

struct AbortFetch<F, T> {
    signal: AbortSignal,
    future: F,
    _rt: PhantomData<T>,
}

impl<F, T> AbortFetch<F, T> {
    fn new(signal: AbortSignal, future: F) -> Self {
        Self {
            signal,
            future,
            _rt: PhantomData,
        }
    }
}

impl<F, T> Future for AbortFetch<F, T>
where
    F: Future + Unpin + 'static,
    F::Output: Into<Result<T, ProviderError>> + 'static,
    T: Unpin,
{
    type Output = Result<T, ProviderError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        if self.signal.aborted() {
            return Poll::Ready(Err(ProviderError::Abort));
        }

        match Pin::new(&mut self.future).poll(cx) {
            Poll::Ready(output) => Poll::Ready(output.into()),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Debug)]
pub enum ProviderError {
    Abort,
    Io(std::io::Error),
    DataUrl(data_url::DataUrlError),
    DataUrlBase64(data_url::forgiving_base64::InvalidBase64),
    ReqwestError(reqwest::Error),
    #[cfg(feature = "cache")]
    ReqwestMiddlewareError(reqwest_middleware::Error),
    HttpStatus {
        status: reqwest::StatusCode,
        url: String,
    },
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Abort => write!(f, "request aborted"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::DataUrl(e) => write!(f, "data url error: {e:?}"),
            Self::DataUrlBase64(e) => write!(f, "data url base64 error: {e:?}"),
            Self::ReqwestError(e) => write!(f, "reqwest error: {e}"),
            #[cfg(feature = "cache")]
            Self::ReqwestMiddlewareError(e) => write!(f, "reqwest middleware error: {e}"),
            Self::HttpStatus { status, url } => write!(f, "HTTP {status} for {url}"),
        }
    }
}

impl From<std::io::Error> for ProviderError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<data_url::DataUrlError> for ProviderError {
    fn from(value: data_url::DataUrlError) -> Self {
        Self::DataUrl(value)
    }
}

impl From<data_url::forgiving_base64::InvalidBase64> for ProviderError {
    fn from(value: data_url::forgiving_base64::InvalidBase64) -> Self {
        Self::DataUrlBase64(value)
    }
}

impl From<reqwest::Error> for ProviderError {
    fn from(value: reqwest::Error) -> Self {
        Self::ReqwestError(value)
    }
}

#[cfg(feature = "cache")]
impl From<reqwest_middleware::Error> for ProviderError {
    fn from(value: reqwest_middleware::Error) -> Self {
        Self::ReqwestMiddlewareError(value)
    }
}

trait ReqwestExt {
    async fn apply_body(self, body: Body, content_type: Option<&str>) -> Self;
}
impl ReqwestExt for RequestBuilder {
    async fn apply_body(self, body: Body, content_type: Option<&str>) -> Self {
        match body {
            Body::Bytes(bytes) => self.body(bytes),
            Body::Form(form_data) => match content_type {
                Some("application/x-www-form-urlencoded") => self.form(&form_data),
                #[cfg(feature = "multipart")]
                Some("multipart/form-data") => {
                    use blitz_traits::net::Entry;
                    use blitz_traits::net::EntryValue;
                    let mut form_data = form_data;
                    let mut form = reqwest::multipart::Form::new();
                    for Entry { name, value } in form_data.0.drain(..) {
                        form = match value {
                            EntryValue::String(value) => form.text(name, value),
                            EntryValue::File(path_buf) => form
                                .file(name, path_buf)
                                .await
                                .expect("Couldn't read form file from disk"),
                            EntryValue::EmptyFile => form.part(
                                name,
                                reqwest::multipart::Part::bytes(&[])
                                    .mime_str("application/octet-stream")
                                    .unwrap(),
                            ),
                        };
                    }
                    self.multipart(form)
                }
                _ => self,
            },
            Body::Empty => self,
        }
    }
}

struct DummyNetWaker;
impl NetWaker for DummyNetWaker {
    fn wake(&self, _client_id: usize) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitz_traits::net::Url;

    /// A `data:` URL states its own mime type, so the synthesised response
    /// carries a real `Content-Type` rather than nothing.
    ///
    /// This is the case that makes the header meaningful for a caller checking
    /// one: a module inlined as a `data:` URL is as legitimate as a fetched
    /// one, and refusing it for having no type would be wrong.
    /// Serve exactly one HTTP request on loopback and hand back what was sent.
    ///
    /// A real socket rather than a mock, because the thing under test is a
    /// header on the wire. Asserting that the provider stored the string it was
    /// given would pass just as happily with the header never attached, which
    /// is the failure this exists to catch.
    fn capture_one_request() -> (Url, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback is available");
        let port = listener
            .local_addr()
            .expect("the socket has an address")
            .port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("the provider connects");
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            // Read only the head. Reading to EOF would block: the client keeps
            // the connection open waiting for the response that follows.
            while !seen.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => seen.push(byte[0]),
                }
            }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            let _ = stream.flush();
            String::from_utf8_lossy(&seen).to_string()
        });

        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).expect("a valid loopback URL");
        (url, handle)
    }

    /// A caller that names a user agent has it sent.
    ///
    /// Sites decide what markup to serve from this header, so a browser that
    /// cannot choose one is served whatever the default is taken for, and on a
    /// large share of the popular web that is a challenge page rather than the
    /// document.
    #[tokio::test]
    async fn a_chosen_user_agent_reaches_the_server() {
        let (url, server) = capture_one_request();
        let provider = Provider::with_user_agent(None, "Chuzz/1.0 (a stated identity)");

        let _ = provider.fetch_async(Request::get(url)).await;

        let request = server
            .join()
            .expect("the server thread finishes")
            .to_lowercase();
        assert!(
            request.contains("user-agent: chuzz/1.0 (a stated identity)"),
            "the chosen user agent should be on the wire, got:\n{request}"
        );
    }

    /// A caller that names none is unchanged by this being configurable.
    #[tokio::test]
    async fn the_default_user_agent_is_still_sent_when_none_is_chosen() {
        let (url, server) = capture_one_request();
        let provider = Provider::new(None);

        let _ = provider.fetch_async(Request::get(url)).await;

        let request = server
            .join()
            .expect("the server thread finishes")
            .to_lowercase();
        assert!(
            request.contains(&format!(
                "user-agent: {}",
                DEFAULT_USER_AGENT.to_lowercase()
            )),
            "the default user agent should be on the wire, got:\n{request}"
        );
    }

    /// A consumer jar participates in the client's whole exchange. The first
    /// request reads its existing cookie, a redirect writes another cookie
    /// before the next request, and the final error response is written too.
    #[cfg(feature = "cookies")]
    #[tokio::test]
    async fn a_consumer_cookie_store_sees_redirects_and_error_responses() {
        use blitz_traits::net::http::HeaderValue;
        use std::io::{Read, Write};

        #[derive(Default)]
        struct RecordingCookieStore {
            request_header: Mutex<String>,
            responses: Mutex<Vec<(String, Vec<String>)>>,
        }

        impl CookieStore for RecordingCookieStore {
            fn set_cookies(
                &self,
                cookie_headers: &mut dyn Iterator<Item = &HeaderValue>,
                url: &Url,
            ) {
                let fields = cookie_headers
                    .filter_map(|header| header.to_str().ok().map(str::to_owned))
                    .collect::<Vec<_>>();
                let mut request_header = self.request_header.lock().unwrap();
                for pair in fields
                    .iter()
                    .filter_map(|field| field.split(';').next())
                    .filter(|pair| !pair.is_empty())
                {
                    if !request_header.is_empty() {
                        request_header.push_str("; ");
                    }
                    request_header.push_str(pair);
                }
                self.responses
                    .lock()
                    .unwrap()
                    .push((url.path().to_owned(), fields));
            }

            fn cookies(&self, _url: &Url) -> Option<HeaderValue> {
                let header = self.request_header.lock().unwrap();
                (!header.is_empty()).then(|| {
                    HeaderValue::from_str(&header).expect("the fixture cookie header is valid")
                })
            }
        }

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener is available");
        let port = listener
            .local_addr()
            .expect("listener has an address")
            .port();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().expect("the provider connects");
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => head.push(byte[0]),
                    }
                }
                requests.push(String::from_utf8_lossy(&head).to_ascii_lowercase());
                let response = if index == 0 {
                    "HTTP/1.1 302 Found\r\nLocation: /finish\r\nSet-Cookie: redirected=one; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 418 I'm a teapot\r\nSet-Cookie: final=two; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                stream
                    .write_all(response.as_bytes())
                    .expect("fixture response writes");
            }
            requests
        });

        let cookies = Arc::new(RecordingCookieStore {
            request_header: Mutex::new("seed=outbound".to_owned()),
            responses: Mutex::new(Vec::new()),
        });
        let provider = Provider::with_user_agent_and_cookie_provider(
            None,
            "FixtureBrowser/1.0",
            Arc::clone(&cookies),
        );
        let result = provider
            .fetch_response_async(Request::get(
                Url::parse(&format!("http://127.0.0.1:{port}/start"))
                    .expect("fixture URL is valid"),
            ))
            .await;

        assert!(matches!(
            result,
            Err(ProviderError::HttpStatus { status, .. }) if status.as_u16() == 418
        ));
        let requests = server.join().expect("fixture server finishes");
        assert!(requests[0].contains("cookie: seed=outbound"));
        assert!(requests[0].contains("user-agent: fixturebrowser/1.0"));
        assert!(requests[1].contains("cookie: seed=outbound; redirected=one"));

        let responses = cookies.responses.lock().unwrap();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].0, "/start");
        assert!(responses[0].1[0].starts_with("redirected=one;"));
        assert_eq!(responses[1].0, "/finish");
        assert!(responses[1].1[0].starts_with("final=two;"));
    }

    #[tokio::test]
    async fn a_data_url_reports_the_mime_type_it_declares() {
        let provider = Provider::new(None);
        let request = Request::get(
            // "hello" as base64, typed as a wasm module.
            Url::parse("data:application/wasm;base64,aGVsbG8=").unwrap(),
        );

        let response = provider
            .fetch_response_async(request)
            .await
            .expect("a data URL resolves without a network");

        assert_eq!(
            response
                .headers
                .get(blitz_traits::platform::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/wasm"),
        );
        assert_eq!(response.body.as_ref(), b"hello");
        assert_eq!(response.status, StatusCode::OK);
    }

    /// A `file:` URL has no server and therefore no headers. The absence has to
    /// be an absence, not an empty string or a guess: a caller requiring a
    /// `Content-Type` must be able to tell "nobody said" from "said the wrong
    /// thing".
    #[tokio::test]
    async fn a_file_url_has_no_content_type_to_report() {
        let path = std::env::temp_dir().join("blitz-net-fetch-response-test.txt");
        std::fs::write(&path, b"file body").expect("a scratch file");

        let provider = Provider::new(None);
        let url = Url::from_file_path(&path).expect("an absolute path");
        let response = provider
            .fetch_response_async(Request::get(url))
            .await
            .expect("a file URL resolves without a network");

        assert!(
            response
                .headers
                .get(blitz_traits::platform::http::header::CONTENT_TYPE)
                .is_none(),
            "a file has no server to declare a type"
        );
        assert_eq!(response.body.as_ref(), b"file body");

        let _ = std::fs::remove_file(&path);
    }

    /// `fetch_async` is untouched by this addition, which is the point of
    /// adding a method rather than widening it: every existing caller keeps the
    /// cheap `(String, Bytes)` shape and allocates no `HeaderMap`.
    #[tokio::test]
    async fn fetch_async_still_returns_the_narrow_shape() {
        let provider = Provider::new(None);
        let (url, bytes) = provider
            .fetch_async(Request::get(
                Url::parse("data:text/plain;base64,aGVsbG8=").unwrap(),
            ))
            .await
            .expect("a data URL resolves without a network");

        assert!(url.starts_with("data:"));
        assert_eq!(bytes.as_ref(), b"hello");
    }
}
