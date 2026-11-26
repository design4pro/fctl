//! A loopback transport for the Codex App Server's Responses provider.
//! The launch profile holds a local capability. Only this transport holds
//! the runner's OAuth credential or can send it to OpenAI.

use anyhow::{Context, Result};
use bytes::Bytes;
use fctl_proto::{Millis, runner::CodexCredential};
use http::header::{self, HeaderMap, HeaderValue};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, Limited, combinators::UnsyncBoxBody};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::{JoinHandle, JoinSet};

const UPSTREAM: &str = "https://api.openai.com/v1/responses";
const LIMIT: &str = "subscription_sharing_usage_limit_exceeded";
const MAX_DIAGNOSTIC: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Limit { reset_at: Option<Millis> },
    CredentialRejected,
}

type Notify = Arc<dyn Fn(Signal) + Send + Sync>;
type Upstream = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;
type Body = UnsyncBoxBody<Bytes, hyper::Error>;

#[derive(Clone, PartialEq, Eq)]
struct Lease {
    header: HeaderValue,
    expires_at: Millis,
}

impl Lease {
    fn new(credential: &CodexCredential) -> Result<Self> {
        anyhow::ensure!(
            !credential.access_token.is_empty() && !credential.access_token.starts_with("sk-"),
            "Expected a ChatGPT access token"
        );
        let mut header = HeaderValue::from_str(&format!("Bearer {}", credential.access_token))
            .context("Invalid ChatGPT access token")?;
        header.set_sensitive(true);
        Ok(Self {
            header,
            expires_at: credential.expires_at,
        })
    }
}

struct Access {
    lease: Lease,
    rejected: bool,
    limited: bool,
}

pub struct Proxy {
    base_url: String,
    access: Arc<Mutex<Access>>,
    task: JoinHandle<()>,
}

impl Proxy {
    pub async fn serve(credential: &CodexCredential, notify: Notify) -> Result<Self> {
        let access = Arc::new(Mutex::new(Access {
            lease: Lease::new(credential)?,
            rejected: false,
            limited: false,
        }));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let route = format!("/{}", fctl_domain::ids::uuid_v4());
        let base_url = format!("http://{}{}", listener.local_addr()?, route);
        let mut tcp = HttpConnector::new();
        tcp.enforce_http(false);
        tcp.set_connect_timeout(Some(Duration::from_secs(10)));
        let tls = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .enable_http2()
            .wrap_connector(tcp);
        let client = Client::builder(TokioExecutor::new()).build(tls);
        let state = access.clone();
        let path = Arc::new(format!("{route}/responses"));
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((socket, _)) = accepted else { break };
                        let (path, client, state, notify) = (
                            path.clone(), client.clone(), state.clone(), notify.clone()
                        );
                        connections.spawn(async move {
                            let service = service_fn(move |request| {
                                let (path, client, state, notify) = (
                                    path.clone(), client.clone(), state.clone(), notify.clone()
                                );
                                async move {
                                    Ok::<_, Infallible>(
                                        forward(request, &path, &client, &state, &notify).await
                                    )
                                }
                            });
                            let _ = http1::Builder::new()
                                .timer(hyper_util::rt::TokioTimer::new())
                                .header_read_timeout(Duration::from_secs(10))
                                .serve_connection(TokioIo::new(socket), service)
                                .await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self {
            base_url,
            access,
            task,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn renew(&self, credential: &CodexCredential) -> Result<bool> {
        let lease = Lease::new(credential)?;
        let mut access = self.access.lock().unwrap_or_else(|e| e.into_inner());
        if access.rejected && access.lease.header == lease.header {
            return Ok(false);
        }
        access.lease = lease;
        access.rejected = false;
        Ok(true)
    }

    pub fn set_limited(&self, limited: bool) {
        self.access
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .limited = limited;
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn forward(
    request: Request<Incoming>,
    path: &str,
    client: &Upstream,
    access: &Arc<Mutex<Access>>,
    notify: &Notify,
) -> Response<Body> {
    if request.method() != Method::POST
        || request.uri().path_and_query().map(|p| p.as_str()) != Some(path)
    {
        return error(StatusCode::NOT_FOUND, "Responses endpoint not found");
    }
    let (parts, body) = request.into_parts();
    let payload = match tokio::time::timeout(
        Duration::from_secs(30),
        Limited::new(body, fctl_proto::agent::MAX_FRAME_BYTES).collect(),
    )
    .await
    {
        Ok(Ok(body)) => body.to_bytes(),
        _ => return error(StatusCode::BAD_REQUEST, "Invalid Responses request body"),
    };
    let Some(payload) = plan_payload(&payload) else {
        return error(StatusCode::BAD_REQUEST, "Invalid Responses request body");
    };
    let lease = {
        let guard = access.lock().unwrap_or_else(|e| e.into_inner());
        if guard.limited {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "ChatGPT plan usage is paused",
            );
        }
        if guard.rejected || guard.lease.expires_at <= super::now_ms() {
            return error(
                StatusCode::UNAUTHORIZED,
                "ChatGPT credential is unavailable",
            );
        }
        guard.lease.clone()
    };
    let builder = Request::post(UPSTREAM)
        .header(header::AUTHORIZATION, lease.header.clone())
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "text/event-stream")
        .header("originator", "fctl");
    let builder = match parts.headers.get(header::USER_AGENT) {
        Some(v) => builder.header(header::USER_AGENT, v),
        None => builder,
    };
    let Ok(request) = builder.body(Full::new(Bytes::from(payload))) else {
        return error(StatusCode::BAD_REQUEST, "Invalid Responses request");
    };
    let upstream =
        match tokio::time::timeout(Duration::from_secs(60), client.request(request)).await {
            Ok(Ok(res)) => res,
            _ => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Could not reach the OpenAI Responses API",
                );
            }
        };
    let (upstream, body) = upstream.into_parts();
    if upstream.status == StatusCode::UNAUTHORIZED {
        let mut guard = access.lock().unwrap_or_else(|e| e.into_inner());
        if guard.lease.header == lease.header && !guard.rejected {
            guard.rejected = true;
            notify(Signal::CredentialRejected);
        }
    }
    let sse = upstream
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|s| s.starts_with("text/event-stream"));
    let mut observation = Observation::new(upstream.status, sse);
    let access = access.clone();
    let notify = notify.clone();
    let body = body
        .map_frame(move |frame| {
            if let Some(data) = frame.data_ref()
                && let Some(signal) = observation.read(data)
            {
                let mut guard = access.lock().unwrap_or_else(|e| e.into_inner());
                if !guard.limited {
                    guard.limited = true;
                    notify(signal);
                }
            }
            frame
        })
        .boxed_unsync();
    let mut result = Response::new(body);
    *result.status_mut() = upstream.status;
    copy_headers(&upstream.headers, result.headers_mut());
    result
}

fn copy_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for (key, value) in from {
        if matches!(
            key.as_str(),
            "content-length" | "content-type" | "content-encoding" | "retry-after" | "x-request-id"
        ) || key.as_str().starts_with("x-ratelimit-")
        {
            to.append(key, value.clone());
        }
    }
}

fn error(status: StatusCode, message: &str) -> Response<Body> {
    let body = json!({"error": {"message": message}}).to_string();
    let mut response = Response::new(
        Full::new(Bytes::from(body))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// Codex 0.172.0 emits tool_search even when no deferred tools are
/// configured. The ChatGPT plan endpoint rejects it and takes local tools
/// from additional_tools input items.
fn plan_payload(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut payload: Value = serde_json::from_slice(bytes).ok()?;
    let body = payload.as_object_mut()?;
    body.insert("store".into(), Value::Bool(false));
    body.insert("stream".into(), Value::Bool(true));
    for key in [
        "background",
        "conversation",
        "max_output_tokens",
        "max_tool_calls",
        "metadata",
        "moderation",
        "multi_agent",
        "previous_response_id",
        "prompt",
        "prompt_cache_retention",
        "safety_identifier",
        "temperature",
        "top_logprobs",
        "top_p",
        "truncation",
        "user",
    ] {
        body.remove(key);
    }
    let mut local = Vec::new();
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        tools.retain_mut(|tool| match tool.get("type").and_then(Value::as_str) {
            Some("function" | "custom") => {
                local.push(tool.take());
                false
            }
            Some("namespace" | "web_search" | "web_search_preview") => true,
            _ => false,
        });
    }
    let input = body.get_mut("input")?.as_array_mut()?;
    for item in input.iter_mut() {
        if item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("role").and_then(Value::as_str) == Some("system")
        {
            item["role"] = json!("developer");
        }
    }
    if !local.is_empty() {
        input.insert(0, json!({"type": "additional_tools", "tools": local}));
    }
    serde_json::to_vec(&payload).ok()
}

enum Observation {
    None,
    Json(Vec<u8>),
    Sse(Sse),
}

impl Observation {
    fn new(status: StatusCode, sse: bool) -> Self {
        if status == StatusCode::TOO_MANY_REQUESTS {
            Self::Json(Vec::new())
        } else if status.is_success() && sse {
            Self::Sse(Sse::default())
        } else {
            Self::None
        }
    }

    fn read(&mut self, bytes: &[u8]) -> Option<Signal> {
        match self {
            Self::None => None,
            Self::Json(pending) => {
                if pending.len() + bytes.len() > MAX_DIAGNOSTIC {
                    *self = Self::None;
                    return None;
                }
                pending.extend_from_slice(bytes);
                let error = serde_json::from_slice::<Value>(pending).ok()?;
                *self = Self::None;
                usage_limit(error.get("error")?)
            }
            Self::Sse(sse) => sse.read(bytes),
        }
    }
}

#[derive(Default)]
struct Sse {
    line: Vec<u8>,
    data: Vec<u8>,
    oversized: bool,
    incomplete_line: bool,
}

impl Sse {
    fn read(&mut self, chunk: &[u8]) -> Option<Signal> {
        for part in chunk.split_inclusive(|&b| b == b'\n') {
            if self.line.len() + part.len() <= MAX_DIAGNOSTIC && !self.incomplete_line {
                self.line.extend_from_slice(part);
            } else {
                self.line.clear();
                self.oversized = true;
                self.incomplete_line = true;
            }
            if !part.ends_with(b"\n") {
                continue;
            }
            if self.incomplete_line {
                self.incomplete_line = false;
                continue;
            }
            let line = self.line.strip_suffix(b"\n").unwrap_or(&self.line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                let limit = (!self.oversized)
                    .then(|| serde_json::from_slice::<Value>(&self.data).ok())
                    .flatten()
                    .filter(|v| v.get("type").and_then(Value::as_str) == Some("response.failed"))
                    .and_then(|v| usage_limit(v.pointer("/response/error")?));
                self.data.clear();
                self.oversized = false;
                self.line.clear();
                if limit.is_some() {
                    return limit;
                }
                continue;
            }
            if let Some(data) = line.strip_prefix(b"data:").filter(|_| !self.oversized) {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                if self.data.len() + data.len() < MAX_DIAGNOSTIC {
                    self.data.extend_from_slice(data);
                    self.data.push(b'\n');
                } else {
                    self.oversized = true;
                }
            }
            self.line.clear();
        }
        None
    }
}

fn usage_limit(error: &Value) -> Option<Signal> {
    if error.get("code").and_then(Value::as_str) != Some(LIMIT) {
        return None;
    }
    let reset_at = ["resets_at", "reset_at"].into_iter().find_map(|key| {
        let at = error.get(key)?.as_i64()?;
        if at >= 1_000_000_000_000 {
            Some(at)
        } else if at >= 1_000_000_000 {
            Some(at * 1000)
        } else {
            None
        }
    });
    Some(Signal::Limit { reset_at })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plan_compatible_tools_and_history_go_upstream() -> Result<()> {
        let payload = json!({
            "store": true, "stream": false, "previous_response_id": "resp_1",
            "tools": [
                {"type": "tool_search"},
                {"type": "function", "name": "exec_command"},
                {"type": "custom", "name": "apply_patch"}
            ],
            "input": [{"type": "message", "role": "system", "content": "instructions"}]
        });
        let bytes = plan_payload(&serde_json::to_vec(&payload)?).context("valid request")?;
        let v: Value = serde_json::from_slice(&bytes)?;
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);
        assert_eq!(v["tools"], json!([]));
        assert!(v.get("previous_response_id").is_none());
        assert_eq!(v["input"][0]["type"], "additional_tools");
        assert_eq!(v["input"][0]["tools"][1]["name"], "apply_patch");
        assert_eq!(v["input"][1]["role"], "developer");
        Ok(())
    }

    #[test]
    fn distinguishes_usage_limits_from_generic_rate_limits() {
        let mut reader = Observation::new(StatusCode::TOO_MANY_REQUESTS, false);
        assert_eq!(
            reader.read(br#"{"error":{"code":"rate_limit_exceeded"}}"#),
            None
        );
        let mut reader = Observation::new(StatusCode::TOO_MANY_REQUESTS, false);
        assert_eq!(
            reader.read(br#"{"error":{"code":"subscription_sharing_usage_limit_"#),
            None
        );
        assert_eq!(
            reader.read(br#"exceeded","resets_at":1795700000}}"#),
            Some(Signal::Limit {
                reset_at: Some(1795700000000)
            })
        );
        let mut reader = Observation::new(StatusCode::OK, true);
        let event = concat!(
            "event: response.output_text.delta\r\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"subscription_sharing_usage_limit_exceeded\"}\r\n\r\n",
            "event: response.failed\r\n",
            "data: {\"type\":\"response.failed\",\r\n",
            "data: \"response\":{\"error\":{\"code\":\"subscription_sharing_usage_limit_exceeded\"}}}\r\n\r\n"
        );
        let signals: Vec<_> = event
            .as_bytes()
            .chunks(3)
            .filter_map(|b| reader.read(b))
            .collect();
        assert_eq!(signals, [Signal::Limit { reset_at: None }]);
    }
}
