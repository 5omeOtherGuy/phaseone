//! The transport seam: one POST in, a streaming body out.
//!
//! `Transport` is the only network-shaped dependency a provider adapter sees, so
//! adapters are testable against [`crate::testing::ScriptedTransport`] with no
//! live network. [`ReqwestTransport`] is the one real implementation.
//!
//! Nothing in this module may place a header value, a request body or a full URL
//! (with its query string) into an error or a `Debug` output: credentials travel
//! in headers, and provider endpoints carry prompt material in query strings.

use std::pin::Pin;
use std::time::Duration;

use futures_core::Stream;
use futures_util::StreamExt;
use p1_contracts::BoxFuture;

/// Bytes of a streaming response body. `Err` ends the body with a transport
/// failure; the message never contains response bytes or a header value.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, TransportError>> + Send>>;

/// A transport failure. The message is safe to log: it names a failure class,
/// never a URL query string, a header value or a body byte.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TransportError(pub String);

/// One request. Every provider route in this slice is `POST` with a JSON body.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for HttpRequest {
    /// Header *values* and the body never appear (the `Authorization` header
    /// carries the credential); the query string is dropped because it may carry
    /// prompt material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("HttpRequest")
            .field("url", &RedactedUrl(&self.url))
            .field("header_names", &names)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// A URL with everything after `?` removed, so a `Debug` never prints a query
/// string. Userinfo (`user:pass@`) is stripped too, defensively.
pub(crate) struct RedactedUrl<'a>(pub(crate) &'a str);

impl std::fmt::Debug for RedactedUrl<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let without_query = self.0.split('?').next().unwrap_or("");
        let without_userinfo = match without_query.split_once("://") {
            Some((scheme, rest)) => match rest.split_once('@') {
                Some((_, host)) => format!("{scheme}://{host}"),
                None => without_query.to_string(),
            },
            None => without_query.to_string(),
        };
        f.write_str(&without_userinfo)
    }
}

/// A non-2xx (or 2xx) response with a streaming body.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ByteStream,
}

impl std::fmt::Debug for HttpResponse {
    /// Header *values* and body bytes never appear; only the status and the
    /// header names are diagnostic.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("header_names", &names)
            .finish_non_exhaustive()
    }
}

/// The network seam every provider adapter is written against.
pub trait Transport: Send + Sync {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>>;
}

/// The real transport: rustls, streaming body, no redirects, no cookies, no
/// total request timeout (streams are long-lived; a connect timeout bounds the
/// only wait that could otherwise hang forever).
pub struct ReqwestTransport {
    client: reqwest::Client,
}

/// Connect timeout. A stream has no total timeout, so this is the one bound that
/// keeps an unreachable host from hanging a first attempt forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

impl ReqwestTransport {
    /// Build the shared client. Panics only if the TLS backend cannot start,
    /// which is a programming/environment error, not a request failure.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            // Provider endpoints never redirect; following one would resend the
            // Authorization header to an attacker-controlled host.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build the provider HTTP client");
        Self { client }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for ReqwestTransport {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            let mut builder = self.client.post(&request.url);
            for (name, value) in request.headers {
                builder = builder.header(name, value);
            }
            let response = builder
                .body(request.body)
                .send()
                .await
                .map_err(transport_error)?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_string(),
                        value.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect();
            let body = response
                .bytes_stream()
                .map(|chunk| chunk.map(|bytes| bytes.to_vec()).map_err(transport_error));
            Ok(HttpResponse {
                status,
                headers,
                body: Box::pin(body),
            })
        })
    }
}

/// Map a reqwest failure to a safe message. The failure *class* is enough to act
/// on (retry vs fail); the URL, the query string and every header value are
/// deliberately dropped.
fn transport_error(error: reqwest::Error) -> TransportError {
    let class = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_redirect() {
        "redirect"
    } else if error.is_request() {
        "request"
    } else {
        "other"
    };
    TransportError(format!("HTTP transport error ({class})"))
}
