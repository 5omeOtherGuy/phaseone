//! A scripted transport for tests, behind the cargo feature `testing`.
//!
//! This crate's own tests see it through `cfg(test)`; other crates enable the
//! `testing` feature. It is never linked into a production build.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use futures_util::stream;
use p1_contracts::BoxFuture;

use crate::http::{ByteStream, HttpRequest, HttpResponse, Transport, TransportError};

/// A queue of canned responses plus a record of every request received.
///
/// `Clone` shares the queue and the record, so a clone handed to a provider
/// still reports the requests the provider sent.
#[derive(Clone, Debug)]
pub struct ScriptedTransport {
    inner: Arc<Mutex<ScriptedState>>,
}

#[derive(Debug)]
struct ScriptedState {
    responses: VecDeque<ScriptedResponse>,
    requests: Vec<HttpRequest>,
}

impl ScriptedTransport {
    /// Script `responses`, returned one per `post` in order.
    pub fn new(responses: Vec<ScriptedResponse>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ScriptedState {
                responses: responses.into(),
                requests: Vec::new(),
            })),
        }
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.lock().requests.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ScriptedState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Transport for ScriptedTransport {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        let response = {
            let mut state = self.lock();
            state.requests.push(request);
            let request_number = state.requests.len();
            state.responses.pop_front().unwrap_or_else(|| {
                panic!(
                    "ScriptedTransport: no scripted response left for request #{request_number}; \
                     script one response per expected request"
                )
            })
        };
        Box::pin(async move { response.into_http_response() })
    }
}

/// One canned response: a status, headers, body chunks and how the body ends.
#[derive(Clone, Debug)]
pub struct ScriptedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub chunks: Vec<Vec<u8>>,
    pub end: BodyEnd,
}

/// How a scripted body terminates.
#[derive(Clone, Debug)]
pub enum BodyEnd {
    /// Clean EOF.
    Eof,
    /// The body read fails with this message (a broken stream).
    Error(String),
    /// The body never produces another byte (a stalled provider).
    Hang,
}

impl ScriptedResponse {
    /// A response to a request that never reached the server: `post` itself
    /// fails. Encoded as status `0`, which no HTTP response can have.
    pub fn connect_error(message: impl Into<String>) -> Self {
        Self {
            status: 0,
            headers: Vec::new(),
            chunks: Vec::new(),
            end: BodyEnd::Error(message.into()),
        }
    }

    /// `200` with the whole SSE body in one chunk, ending cleanly.
    pub fn ok_sse(body: &str) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            chunks: vec![body.as_bytes().to_vec()],
            end: BodyEnd::Eof,
        }
    }

    /// `200` with the SSE body split into two chunks at byte offset `at`.
    pub fn ok_sse_split(body: &str, at: usize) -> Self {
        let bytes = body.as_bytes();
        let at = at.min(bytes.len());
        Self {
            status: 200,
            headers: Vec::new(),
            chunks: vec![bytes[..at].to_vec(), bytes[at..].to_vec()],
            end: BodyEnd::Eof,
        }
    }

    fn into_http_response(self) -> Result<HttpResponse, TransportError> {
        let Self {
            status,
            headers,
            chunks,
            end,
        } = self;
        if status == 0 {
            let message = match end {
                BodyEnd::Error(message) => message,
                _ => "scripted connect error".to_string(),
            };
            return Err(TransportError(message));
        }
        Ok(HttpResponse {
            status,
            headers,
            body: body_stream(chunks, end),
        })
    }
}

fn body_stream(chunks: Vec<Vec<u8>>, end: BodyEnd) -> ByteStream {
    let chunks = chunks.into_iter().map(Ok::<Vec<u8>, TransportError>);
    match end {
        BodyEnd::Eof => Box::pin(stream::iter(chunks)),
        BodyEnd::Error(message) => Box::pin(stream::iter(
            chunks.chain(std::iter::once(Err(TransportError(message)))),
        )),
        BodyEnd::Hang => Box::pin(
            stream::iter(chunks).chain(stream::pending::<Result<Vec<u8>, TransportError>>()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use super::*;

    #[tokio::test]
    async fn records_requests_and_replays_responses_in_order() {
        let transport = ScriptedTransport::new(vec![
            ScriptedResponse::ok_sse("data: one\n\n"),
            ScriptedResponse::ok_sse("data: two\n\n"),
        ]);
        let request = |url: &str| HttpRequest {
            url: url.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        };

        let first = transport.post(request("https://a.test")).await.unwrap();
        assert_eq!(first.status, 200);
        let second = transport.post(request("https://b.test")).await.unwrap();
        assert_eq!(second.status, 200);

        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].url, "https://a.test");
        assert_eq!(requests[1].url, "https://b.test");
    }

    #[tokio::test]
    async fn connect_error_fails_post_instead_of_returning_a_response() {
        let transport = ScriptedTransport::new(vec![ScriptedResponse::connect_error("refused")]);
        let error = transport
            .post(HttpRequest {
                url: "https://a.test".to_string(),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await
            .unwrap_err();
        assert_eq!(error, TransportError("refused".to_string()));
    }

    #[tokio::test]
    async fn error_body_ends_the_stream_with_a_transport_error() {
        let response = ScriptedResponse {
            status: 200,
            headers: Vec::new(),
            chunks: vec![b"data: x\n\n".to_vec()],
            end: BodyEnd::Error("reset".to_string()),
        }
        .into_http_response()
        .unwrap();
        let mut body = response.body;
        assert_eq!(body.next().await.unwrap().unwrap(), b"data: x\n\n".to_vec());
        assert_eq!(body.next().await.unwrap().unwrap_err().0, "reset");
        assert!(body.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn hang_body_never_yields_after_its_chunks() {
        let response = ScriptedResponse {
            status: 200,
            headers: Vec::new(),
            chunks: Vec::new(),
            end: BodyEnd::Hang,
        }
        .into_http_response()
        .unwrap();
        let mut body = response.body;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), body.next())
                .await
                .is_err(),
            "a hanging body must not produce an item"
        );
    }

    #[tokio::test]
    async fn split_sse_preserves_the_body_bytes() {
        let body = "data: hello\n\n";
        let response = ScriptedResponse::ok_sse_split(body, 5);
        let mut stream = response.into_http_response().unwrap().body;
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(bytes, body.as_bytes());
    }

    #[tokio::test]
    #[should_panic(expected = "no scripted response left")]
    async fn panics_when_asked_for_more_responses_than_scripted() {
        let transport = ScriptedTransport::new(Vec::new());
        let _ = transport
            .post(HttpRequest {
                url: "https://a.test".to_string(),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await;
    }
}
