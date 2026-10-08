//! Native GitHub GET transport. Guests never choose an origin or see credentials.
use crate::{
    capabilities::{CallState, check_arity},
    loader::interface_import,
};
use p1_contracts::{BoxFuture, CancellationToken};
use reqwest::{
    Client, Url,
    header::{AUTHORIZATION, HeaderValue},
};
use std::time::Duration;
use wasmtime::{
    bail,
    component::{Linker, Val},
};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const ORIGIN: &str = "https://api.github.com";

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum GithubError {
    #[error("GitHub request cancelled")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
}
pub trait GithubService: Send + Sync {
    fn get(
        &self,
        path: String,
        raw: bool,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<String, GithubError>>;
}

/// No Debug implementation: the host-held credential must never enter diagnostics.
pub struct GithubCapability {
    token: Option<String>,
}
impl GithubCapability {
    pub fn new(token: Option<String>) -> Self {
        Self { token }
    }
}

fn failed(message: &str) -> GithubError {
    GithubError::Failed(message.into())
}

/// Validate before attaching credentials. Redirects are refused separately, so
/// neither URL normalization nor a server response can move credentials elsewhere.
fn endpoint(path: &str, raw: bool) -> Result<Url, GithubError> {
    if path.len() > 16_384
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains(['#', '\\'])
        || !path.is_ascii()
        || path.bytes().any(|b| b.is_ascii_control())
    {
        return Err(failed("invalid GitHub API path"));
    }
    let route = path.split('?').next().unwrap_or_default();
    let segments: Vec<_> = route.trim_end_matches('/').split('/').collect();
    let repo = segments.len() >= 4
        && segments[1] == "repos"
        && segments[2..4].iter().all(|s| {
            !s.is_empty()
                && *s != "."
                && *s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        });
    let contents = repo && segments.get(4) == Some(&"contents");
    let allowed = match segments.as_slice() {
        ["", "user", "repos"] => true,
        ["", "search", "code" | "commits" | "repositories"] => true,
        ["", "repos", _, _] => repo,
        ["", "repos", _, _, "commits"] => repo,
        ["", "repos", _, _, "compare", value] => repo && !value.is_empty(),
        ["", "repos", _, _, "git", "trees", value] => repo && !value.is_empty(),
        _ => contents,
    };
    if !allowed || raw && !contents {
        return Err(failed("GitHub endpoint is not granted"));
    }
    let url =
        Url::parse(&format!("{ORIGIN}{path}")).map_err(|_| failed("invalid GitHub API path"))?;
    if url.origin().ascii_serialization() != ORIGIN || url.path() != route {
        return Err(failed("GitHub path normalization is not allowed"));
    }
    Ok(url)
}
fn append(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), GithubError> {
    if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
        return Err(failed("GitHub response exceeds 8 MiB; narrow the request"));
    }
    body.extend_from_slice(chunk);
    Ok(())
}
impl GithubService for GithubCapability {
    fn get(
        &self,
        path: String,
        raw: bool,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<String, GithubError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(GithubError::Cancelled);
            }
            let url = endpoint(&path, raw)?;
            let request = async {
                let client = Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(30))
                    .build()
                    .map_err(|_| failed("cannot initialize GitHub transport"))?;
                let mut request = client
                    .get(url)
                    .header("User-Agent", "p1-github-tools")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .header(
                        "Accept",
                        if raw {
                            "application/vnd.github.raw+json"
                        } else {
                            "application/vnd.github.text-match+json"
                        },
                    );
                if let Some(token) = &self.token {
                    let mut header = HeaderValue::from_str(&format!("Bearer {token}"))
                        .map_err(|_| failed("invalid host GitHub credential"))?;
                    header.set_sensitive(true);
                    request = request.header(AUTHORIZATION, header);
                }
                let mut response = request
                    .send()
                    .await
                    .map_err(|_| failed("GitHub request failed or timed out"))?;
                let status = response.status().as_u16();
                if status != 200 {
                    return Err(GithubError::Failed(match status {
                        401 => "GitHub authentication required".into(),
                        403|429 => "GitHub access denied or rate limited; check token permissions or retry after reset".into(),
                        404 => "GitHub resource not found or inaccessible".into(),
                        422 => "GitHub rejected the query or revision".into(),
                        300..=399 => "GitHub redirect refused; use current repository coordinates".into(),
                        _ => format!("GitHub returned HTTP {status}"),
                    }));
                }
                if response
                    .content_length()
                    .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
                {
                    return Err(failed("GitHub response exceeds 8 MiB; narrow the request"));
                }
                let mut body = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|_| failed("GitHub response failed or timed out"))?
                {
                    append(&mut body, &chunk)?;
                }
                String::from_utf8(body).map_err(|_| failed("GitHub response is not UTF-8 text"))
            };
            tokio::select! {biased; ()=cancel.cancelled()=>Err(GithubError::Cancelled), result=request=>result}
        })
    }
}
pub(crate) fn link(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    linker.instance(&interface_import("github-api"))?.func_new_async("get", |store,_ty,params,results| {
        Box::new(async move {
            check_arity("github-api.get",params,results,2,1)?;
            let [Val::String(path),Val::Bool(raw)]=params else {bail!("github-api.get: invalid arguments");};
            let service=store.data().github.clone().ok_or_else(||wasmtime::format_err!("github-api.get: missing service"))?;
            let cancel=store.data().cancel.clone();
            let result=tokio::select! {biased; ()=cancel.cancelled()=>Err(GithubError::Cancelled), r=service.get(path.clone(),*raw,cancel.clone())=>r};
            results[0]=Val::Result(match result {
                Ok(s)=>Ok(Some(Box::new(Val::String(s)))),
                Err(error)=>Err(Some(Box::new(match error {
                    GithubError::Cancelled=>Val::Variant("cancelled".into(),None),
                    GithubError::Failed(s)=>Val::Variant("failed".into(),Some(Box::new(Val::String(s)))),
                }))),
            });
            Ok(())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_github_research_get_endpoints_are_granted() {
        for (path, raw) in [
            ("/repos/o/r", false),
            ("/repos/o/r/contents/a%20b?ref=feature%2Fx", true),
            ("/repos/o/r/git/trees/main?recursive=1", false),
            ("/search/code?q=word%20repo%3Ao%2Fr", false),
            ("/user/repos?page=2", false),
        ] {
            let url = endpoint(path, raw).unwrap();
            assert_eq!(url.host_str(), Some("api.github.com"));
        }
        for path in [
            "https://example.com",
            "//example.com",
            "/repos/o/r/contents/../issues",
            "/repos/o/r/contents/%2e%2e/issues",
            "/repos/o/r/issues",
            "/repos/o/r/actions/workflows",
            "/user",
            "/repos/o/r#fragment",
            "/repos/o/r\\evil",
            "/search/code\n",
        ] {
            assert!(endpoint(path, false).is_err(), "{path}");
        }
        assert!(endpoint("/search/code", true).is_err());
    }
    #[test]
    fn response_bound_is_enforced_before_appending() {
        let mut body = vec![b'x'; MAX_RESPONSE_BYTES - 2];
        append(&mut body, b"ab").unwrap();
        assert!(append(&mut body, b"c").is_err());
        assert_eq!(body.len(), MAX_RESPONSE_BYTES);
    }
    #[tokio::test]
    async fn cancelled_transport_performs_no_request() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            GithubCapability::new(None)
                .get("/repos/o/r".into(), false, cancel)
                .await,
            Err(GithubError::Cancelled)
        );
    }
}
