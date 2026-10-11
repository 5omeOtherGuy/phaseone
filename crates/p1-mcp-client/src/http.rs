//! The streamable HTTP transport: every client message is one POST to the server's endpoint;
//! a request is answered as one JSON body or as an SSE stream that ends with the answer.
//!
//! The session id the server assigns at `initialize` (`Mcp-Session-Id`) and the negotiated
//! protocol version (`MCP-Protocol-Version`) go with every later message. Redirects are not
//! followed, so a configured header never reaches another host. Error messages carry the
//! status, never a header or a body.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use p1_contracts::mcp::McpError;
use p1_contracts::{BoxFuture, CancellationToken};
use p1_provider_http::SseDecoder;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Response, StatusCode};
use serde_json::Value;

use crate::rpc::{self, Incoming};
use crate::{Channel, MAX_MESSAGE_BYTES};

const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
/// How long reaching the server may take; a call itself is bounded only by cancellation.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct HttpChannel {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    next_id: AtomicU64,
    session: Mutex<Session>,
}

#[derive(Default)]
struct Session {
    id: Option<String>,
    version: Option<String>,
}

impl HttpChannel {
    pub(crate) fn new(url: &str, headers: &BTreeMap<String, String>) -> Result<Self, McpError> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(McpError::Connect(
                "the server URL is not an http or https URL".into(),
            ));
        }
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| McpError::Connect(format!("`{name}` is not a header name")))?;
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| McpError::Connect(format!("the value of `{name}` is not valid")))?;
            value.set_sensitive(true);
            map.insert(name, value);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|error| McpError::Connect(format!("cannot build the HTTP client: {error}")))?;
        Ok(Self {
            client,
            url: url.to_owned(),
            headers: map,
            next_id: AtomicU64::new(1),
            session: Mutex::new(Session::default()),
        })
    }

    fn session(&self) -> MutexGuard<'_, Session> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn post(&self, message: &Value) -> Result<Response, McpError> {
        let (session, version) = {
            let session = self.session();
            (session.id.clone(), session.version.clone())
        };
        let mut request = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(message.to_string());
        if let Some(session) = session {
            request = request.header(SESSION_HEADER, session);
        }
        if let Some(version) = version {
            request = request.header(VERSION_HEADER, version);
        }
        let response = request.send().await.map_err(|error| {
            McpError::Connect(format!("cannot reach the server: {}", error.without_url()))
        })?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND && self.session().id.is_some() {
            return Err(McpError::Closed("the server ended the session".into()));
        }
        if !status.is_success() {
            return Err(McpError::Protocol(format!(
                "the server answered HTTP {status}"
            )));
        }
        Ok(response)
    }

    async fn exchange(&self, id: u64, method: &str, params: Value) -> Result<Value, McpError> {
        let response = self.post(&rpc::request(id, method, params)).await?;
        if method == "initialize"
            && let Some(session) = response
                .headers()
                .get(SESSION_HEADER)
                .and_then(|value| value.to_str().ok())
        {
            self.session().id = Some(session.to_owned());
        }
        let streamed = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|kind| kind.starts_with("text/event-stream"));
        if streamed {
            self.read_stream(id, response).await
        } else {
            let body = read_body(response).await?;
            let message: Value = serde_json::from_slice(&body).map_err(|error| {
                McpError::Protocol(format!("the server's answer is not JSON: {error}"))
            })?;
            let messages = match message {
                Value::Array(batch) => batch,
                single => vec![single],
            };
            for message in messages {
                if let Some(answer) = self.place(id, message).await? {
                    return answer;
                }
            }
            Err(McpError::Protocol(
                "the server's answer does not answer the request".into(),
            ))
        }
    }

    async fn read_stream(&self, id: u64, mut response: Response) -> Result<Value, McpError> {
        let mut decoder = SseDecoder::new();
        loop {
            let chunk = response.chunk().await.map_err(|error| {
                McpError::Closed(format!("the stream broke: {}", error.without_url()))
            })?;
            let Some(chunk) = chunk else {
                return Err(McpError::Closed(
                    "the stream ended before the answer".into(),
                ));
            };
            let events = decoder
                .try_push(&chunk)
                .map_err(|error| McpError::Protocol(error.to_string()))?;
            for event in events {
                if event.data.trim().is_empty() {
                    continue;
                }
                let message: Value = serde_json::from_str(&event.data).map_err(|error| {
                    McpError::Protocol(format!("a stream event is not JSON: {error}"))
                })?;
                if let Some(answer) = self.place(id, message).await? {
                    return answer;
                }
            }
        }
    }

    /// Route one message the server sent while the client waits for `id`: the answer is
    /// returned, a server request is answered, anything else is skipped.
    async fn place(
        &self,
        id: u64,
        message: Value,
    ) -> Result<Option<Result<Value, McpError>>, McpError> {
        match rpc::classify(message) {
            Incoming::Response {
                id: answered,
                result,
            } if answered == id => Ok(Some(result)),
            Incoming::Response { .. } | Incoming::Notification => Ok(None),
            Incoming::Request { id, method } => {
                self.post(&rpc::answer(id, &method)).await?;
                Ok(None)
            }
            Incoming::Invalid(reason) => Err(McpError::Protocol(reason)),
        }
    }
}

impl Channel for HttpChannel {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(async move {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            tokio::select! {
                answer = self.exchange(id, method, params) => answer,
                () = cancel.cancelled() => {
                    // Dropping the exchange closed its stream; tell the server too.
                    let _ = self.post(&rpc::notification(
                        "notifications/cancelled",
                        serde_json::json!({ "requestId": id, "reason": "cancelled by p1" }),
                    )).await;
                    Err(McpError::Cancelled)
                }
            }
        })
    }

    /// Every later message names the version `initialize` negotiated.
    fn negotiated(&self, version: &str) {
        self.session().version = Some(version.to_owned());
    }

    fn notify<'a>(&'a self, method: &'a str, params: Value) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            self.post(&rpc::notification(method, params)).await?;
            Ok(())
        })
    }
}

/// A JSON answer body, bounded by [`MAX_MESSAGE_BYTES`].
async fn read_body(mut response: Response) -> Result<Vec<u8>, McpError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| McpError::Closed(format!("the answer broke: {}", error.without_url())))?
    {
        if body.len() + chunk.len() > MAX_MESSAGE_BYTES {
            return Err(McpError::Protocol(format!(
                "the server's answer is larger than {MAX_MESSAGE_BYTES} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
