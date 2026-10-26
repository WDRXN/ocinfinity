use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::config::ProxyConfig;
use crate::proxy::server::build_client;
use crate::transform::{
    anthropic_to_opencode_request, opencode_response_to_anthropic, SseTranslator,
};
use crate::warp::WarpResolver;

pub struct ProxyState {
    pub config: ProxyConfig,
    pub client: Client,
    pub warp_resolver: WarpResolver,
}

/// Which wire format the *client* is speaking.
///
/// OpenCode Zen is OpenAI-compatible, so requests are always translated into
/// OpenAI shape on the way out. On the way back the caller gets whatever it
/// asked for: returning Anthropic JSON to an OpenAI client on
/// `/v1/chat/completions` (the previous behaviour) broke every non-Claude
/// consumer of that endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFormat {
    Anthropic,
    Openai,
}

fn bearer(api_key: &Option<String>, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match api_key {
        Some(key) if !key.is_empty() => request.header("Authorization", format!("Bearer {}", key)),
        _ => request,
    }
}

pub async fn handle_models(State(state): State<Arc<Mutex<ProxyState>>>) -> impl IntoResponse {
    let (base_url, api_key, client) = {
        let guard = state.lock().await;
        (
            guard.config.opencode_base_url.clone(),
            guard.config.opencode_api_key.clone(),
            guard.client.clone(),
        )
    };

    let models_url = crate::detect::endpoint(&base_url, "/v1/models");

    let request = bearer(
        &api_key,
        client.get(&models_url).header("Accept", "application/json"),
    );

    match request.send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
            Ok(upstream_models) => (
                StatusCode::OK,
                Json(transform_models_to_anthropic(&upstream_models)),
            ),
            Err(e) => {
                error!("failed to parse upstream model list: {}", e);
                (StatusCode::OK, Json(ProxyConfig::models_response()))
            }
        },
        Ok(resp) => {
            warn!("upstream /v1/models returned {}", resp.status());
            (StatusCode::OK, Json(ProxyConfig::models_response()))
        }
        Err(e) => {
            warn!("upstream unreachable at {}: {}", base_url, e);
            (StatusCode::OK, Json(ProxyConfig::models_response()))
        }
    }
}

fn title_case(id: &str) -> String {
    id.replace('-', " ")
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => format!(
                    "{}{}",
                    first.to_uppercase().collect::<String>(),
                    chars.as_str()
                ),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn transform_models_to_anthropic(upstream: &Value) -> Value {
    let models: Vec<Value> = match upstream.get("data").and_then(Value::as_array) {
        Some(entries) => entries
            .iter()
            .filter_map(|m| {
                let id = m.get("id").and_then(Value::as_str).unwrap_or("");
                if id.is_empty() {
                    return None;
                }

                let display_name = m
                    .get("name")
                    .or_else(|| m.get("display_name"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| title_case(id));

                let context_window = m
                    .get("context_window")
                    .and_then(Value::as_u64)
                    .or_else(|| m.get("max_tokens").and_then(Value::as_u64))
                    .unwrap_or(128_000);

                let mut model = serde_json::Map::new();
                model.insert("id".to_string(), Value::String(id.to_string()));
                model.insert("type".to_string(), Value::String("model".to_string()));
                model.insert("name".to_string(), Value::String(display_name.clone()));
                model.insert("display_name".to_string(), Value::String(display_name));
                model.insert(
                    "context_window".to_string(),
                    Value::Number(serde_json::Number::from(context_window)),
                );

                for passthrough in ["created", "owned_by", "architecture"] {
                    if let Some(v) = m.get(passthrough) {
                        model.insert(passthrough.to_string(), v.clone());
                    }
                }

                Some(Value::Object(model))
            })
            .collect(),
        // Upstream did not answer with an OpenAI model list; advertise the
        // known free tier so `/models` in Claude Code is never empty.
        None => ProxyConfig::free_models()
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "name": m.display_name,
                    "type": "model",
                    "display_name": m.display_name,
                    "context_window": 128000,
                })
            })
            .collect(),
    };

    let first = models
        .first()
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let last = models
        .last()
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");

    serde_json::json!({
        "object": "list",
        "data": models,
        "has_more": false,
        "first_id": first,
        "last_id": last,
    })
}

pub async fn handle_model_detail(
    State(state): State<Arc<Mutex<ProxyState>>>,
    axum::extract::Path(model_id): axum::extract::Path<String>,
) -> Response {
    let (base_url, api_key, client) = {
        let guard = state.lock().await;
        (
            guard.config.opencode_base_url.clone(),
            guard.config.opencode_api_key.clone(),
            guard.client.clone(),
        )
    };

    // Encode the path segment so ids containing `/` or spaces survive the hop.
    let encoded = urlencoding_lite(&model_id);
    let model_url = crate::detect::endpoint(&base_url, &format!("/v1/models/{}", encoded));

    let request = bearer(
        &api_key,
        client.get(&model_url).header("Accept", "application/json"),
    );

    match request.send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
            Ok(model) => {
                let mut transformed = serde_json::Map::new();
                let id = model
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id.as_str());
                let name = model
                    .get("name")
                    .or_else(|| model.get("display_name"))
                    .and_then(Value::as_str)
                    .unwrap_or(id);

                transformed.insert("id".to_string(), Value::String(id.to_string()));
                transformed.insert("type".to_string(), Value::String("model".to_string()));
                transformed.insert("name".to_string(), Value::String(name.to_string()));
                transformed.insert("display_name".to_string(), Value::String(name.to_string()));

                (StatusCode::OK, Json(Value::Object(transformed))).into_response()
            }
            Err(e) => {
                error!("failed to parse model detail: {}", e);
                error_json(
                    StatusCode::BAD_GATEWAY,
                    "upstream returned an unparsable model",
                )
            }
        },
        Ok(_) => error_json(
            StatusCode::NOT_FOUND,
            &format!("model `{}` not found upstream", model_id),
        ),
        Err(e) => {
            warn!("model detail fetch failed: {}", e);
            error_json(StatusCode::BAD_GATEWAY, "upstream unreachable")
        }
    }
}

/// Minimal percent-encoding for a single path segment.
fn urlencoding_lite(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

pub async fn handle_messages_anthropic(
    State(state): State<Arc<Mutex<ProxyState>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_messages(state, headers, body, ApiFormat::Anthropic).await
}

pub async fn handle_messages_openai(
    State(state): State<Arc<Mutex<ProxyState>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_messages(state, headers, body, ApiFormat::Openai).await
}

async fn handle_messages(
    state: Arc<Mutex<ProxyState>>,
    _headers: HeaderMap,
    body: Bytes,
    format: ApiFormat,
) -> Response {
    let request_body: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            error!("failed to parse request body: {}", e);
            return error_json(StatusCode::BAD_REQUEST, &format!("invalid JSON: {}", e));
        }
    };

    let is_stream = request_body
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let model = request_body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let opencode_body = anthropic_to_opencode_request(&request_body);

    let (base_url, api_key, max_retries, reset_delay) = {
        let guard = state.lock().await;
        (
            guard.config.opencode_base_url.clone(),
            guard.config.opencode_api_key.clone(),
            guard.config.max_retries,
            guard.config.warp_reset_delay_ms,
        )
    };

    let mut attempt = 0u32;

    loop {
        let result = if is_stream {
            forward_streaming(&base_url, &api_key, &opencode_body, &model, &state, format).await
        } else {
            forward_non_streaming(&base_url, &api_key, &opencode_body, &model, &state, format).await
        };

        match result {
            Ok(response) => return response,
            Err(ProxyError::RateLimited) => {
                attempt += 1;
                if attempt > max_retries {
                    warn!("rate limit persisted after {} WARP resets", max_retries);
                    return error_json(
                        StatusCode::TOO_MANY_REQUESTS,
                        "rate limit still active after WARP resets",
                    );
                }

                info!(
                    "HTTP 429 from upstream, rotating WARP identity (attempt {}/{})",
                    attempt, max_retries
                );

                let resolver = WarpResolver::new(max_retries, reset_delay);
                if !resolver.handle_429(attempt - 1).await {
                    return error_json(
                        StatusCode::TOO_MANY_REQUESTS,
                        "WARP reset failed, rate limit still active",
                    );
                }

                // Drop the pooled connections: they were opened over the old
                // egress address and would keep pinning us to it.
                let fresh = match build_client() {
                    Ok(c) => c,
                    Err(e) => {
                        error!("failed to rebuild HTTP client after reset: {}", e);
                        return error_json(StatusCode::BAD_GATEWAY, "http client rebuild failed");
                    }
                };
                state.lock().await.client = fresh;
            }
            Err(ProxyError::RequestFailed(msg, status)) => {
                error!("upstream request failed: {}", msg);
                return error_json(status, &msg);
            }
        }
    }
}

async fn current_client(state: &Arc<Mutex<ProxyState>>) -> Client {
    state.lock().await.client.clone()
}

async fn forward_streaming(
    base_url: &str,
    api_key: &Option<String>,
    body: &Value,
    model: &str,
    state: &Arc<Mutex<ProxyState>>,
    format: ApiFormat,
) -> Result<Response, ProxyError> {
    let url = crate::detect::endpoint(base_url, "/v1/chat/completions");
    let client = current_client(state).await;

    let request = bearer(
        api_key,
        client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(body),
    );

    let response = request
        .send()
        .await
        .map_err(|e| ProxyError::RequestFailed(e.to_string(), StatusCode::BAD_GATEWAY))?;

    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(ProxyError::RateLimited);
    }
    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        return Err(ProxyError::RequestFailed(
            format!("upstream {}: {}", status, detail),
            StatusCode::BAD_GATEWAY,
        ));
    }

    let stream = match format {
        // OpenAI in, OpenAI out: forward the bytes untouched.
        ApiFormat::Openai => Body::from_stream(
            response
                .bytes_stream()
                .map(|chunk| chunk.map_err(std::io::Error::other)),
        ),
        // OpenAI SSE in, Anthropic SSE out, with line reassembly so a chunk
        // boundary in the middle of a frame cannot corrupt the transcript.
        ApiFormat::Anthropic => {
            let upstream = response.bytes_stream();
            let translator = SseTranslator::new(model);

            // `unfold` owns both the upstream body and the translator, so the
            // resulting stream is `'static` and can be handed to axum. The
            // trailing `done` flag guarantees termination: without it the
            // terminal branch would keep yielding empty frames forever.
            let translated = futures::stream::unfold(
                (upstream, translator, false),
                |(mut upstream, mut translator, mut done)| async move {
                    if done {
                        return None;
                    }
                    loop {
                        match upstream.next().await {
                            Some(Ok(chunk)) => {
                                let out = translator.feed(&chunk);
                                if out.is_empty() {
                                    // Nothing emit-able yet; read the next chunk.
                                    continue;
                                }
                                return Some((
                                    Ok::<Bytes, std::io::Error>(Bytes::from(out)),
                                    (upstream, translator, done),
                                ));
                            }
                            // An upstream error or early close still has to
                            // terminate the Anthropic stream properly, otherwise
                            // Claude Code hangs waiting for `message_stop`.
                            other => {
                                if let Some(Err(e)) = other {
                                    tracing::warn!("upstream stream ended early: {}", e);
                                }
                                done = true;
                                let out = translator.close();
                                return Some((Ok(Bytes::from(out)), (upstream, translator, done)));
                            }
                        }
                    }
                },
            );

            Body::from_stream(translated)
        }
    };

    let mut response = Response::new(stream);
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        "Content-Type",
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert("Cache-Control", HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert("X-Accel-Buffering", HeaderValue::from_static("no"));

    Ok(response)
}

async fn forward_non_streaming(
    base_url: &str,
    api_key: &Option<String>,
    body: &Value,
    model: &str,
    state: &Arc<Mutex<ProxyState>>,
    format: ApiFormat,
) -> Result<Response, ProxyError> {
    let url = crate::detect::endpoint(base_url, "/v1/chat/completions");
    let client = current_client(state).await;

    let request = bearer(
        api_key,
        client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(body),
    );

    let response = request
        .send()
        .await
        .map_err(|e| ProxyError::RequestFailed(e.to_string(), StatusCode::BAD_GATEWAY))?;

    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(ProxyError::RateLimited);
    }
    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        return Err(ProxyError::RequestFailed(
            format!("upstream {}: {}", status, detail),
            StatusCode::BAD_GATEWAY,
        ));
    }

    let payload = response
        .json::<Value>()
        .await
        .map_err(|e| ProxyError::RequestFailed(e.to_string(), StatusCode::BAD_GATEWAY))?;

    let request_id = format!(
        "req_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        headers.insert("x-request-id", value);
    }

    let body_value = match format {
        ApiFormat::Anthropic => opencode_response_to_anthropic(&payload, model),
        ApiFormat::Openai => payload,
    };

    Ok((StatusCode::OK, headers, Json(body_value)).into_response())
}

/// Build an error body in the shape the calling client expects, so an
/// Anthropic client gets an Anthropic error envelope instead of a bare string.
fn error_json(status: StatusCode, message: &str) -> Response {
    let error = serde_json::json!({
        "type": "error",
        "error": {
            "type": match status {
                StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
                StatusCode::BAD_REQUEST => "invalid_request_error",
                StatusCode::NOT_FOUND => "not_found_error",
                _ => "api_error",
            },
            "message": message,
        }
    });

    (status, Json(error)).into_response()
}

#[derive(Debug)]
enum ProxyError {
    RateLimited,
    RequestFailed(String, StatusCode),
}
