use axum::{
    middleware::{self, Next},
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

use crate::config::ProxyConfig;
use crate::proxy::handlers::{
    handle_messages_anthropic, handle_messages_openai, handle_model_detail, handle_models,
    ProxyState,
};
use crate::warp::WarpResolver;

/// Header every response carries so `ocinfinity detect` can recognise its own
/// proxies and never mistake one for the OpenCode upstream.
pub const INSTANCE_HEADER: &str = "x-ocinfinity";

async fn log_requests(
    request: axum::http::Request<axum::body::Body>,
    next: Next,
) -> axum::response::Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    info!("-> {} {}", method, uri);
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(INSTANCE_HEADER, axum::http::HeaderValue::from_static("1"));
    info!("<- {} {} {}", method, uri, response.status());
    response
}

/// Build the shared outbound HTTP client.
///
/// Rustls rather than the platform TLS stack: the Linux build then carries no
/// OpenSSL dependency, and certificate handling is identical on both OSes.
///
/// The user agent is mandatory, not cosmetic: Cloudflare fronts the OpenCode
/// gateway and refuses requests that do not identify a client.
pub fn build_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(300))
        .pool_max_idle_per_host(32)
        .user_agent(crate::detect::USER_AGENT)
        .build()
}

/// Normalise a `--listen` value into a socket address.
///
/// Accepts `9000`, `:9000`, `0.0.0.0:9000` and `host:port`. The previous
/// implementation silently bound to 127.0.0.1:8080 whenever the parse failed,
/// which meant a typo'd `--listen` produced a proxy on the wrong port with no
/// indication anything was wrong.
pub fn resolve_listen_addr(listen: &str) -> Result<SocketAddr, String> {
    let trimmed = listen.trim();

    // Bare port: `9000`.
    if let Ok(port) = trimmed.parse::<u16>() {
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }

    // Leading colon: `:9000`.
    let candidate = if let Some(port) = trimmed.strip_prefix(':') {
        format!("127.0.0.1:{}", port)
    } else {
        trimmed.to_string()
    };

    candidate.parse::<SocketAddr>().map_err(|_| {
        format!(
            "`{}` is not a valid listen address (expected host:port)",
            listen
        )
    })
}

pub async fn start_proxy_server(config: ProxyConfig) -> anyhow::Result<()> {
    let client = build_client()?;
    let addr = resolve_listen_addr(&config.listen_addr).map_err(anyhow::Error::msg)?;

    let warp_resolver = WarpResolver::new(config.max_retries, config.warp_reset_delay_ms);

    let state = Arc::new(Mutex::new(ProxyState {
        config: config.clone(),
        client,
        warp_resolver,
    }));

    let app = Router::new()
        // `/v1/messages` is the Anthropic surface Claude Code speaks.
        .route("/v1/messages", post(handle_messages_anthropic))
        // `/v1/chat/completions` stays OpenAI-shaped end to end.
        .route("/v1/chat/completions", post(handle_messages_openai))
        .route("/v1/models", get(handle_models))
        .route("/v1/models/{model_id}", get(handle_model_detail))
        .route("/health", get(|| async { "OK" }))
        .layer(middleware::from_fn(log_requests))
        .with_state(state);

    info!("OpenCode Zen upstream: {}", config.opencode_base_url);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind {}: {}", addr, e))?;

    info!("proxy listening on http://{}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("proxy shut down");
    Ok(())
}

/// Exit cleanly on Ctrl-C instead of tearing the process down mid-response.
async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_addr_accepts_bare_port() {
        assert_eq!(
            resolve_listen_addr("9000").unwrap(),
            "127.0.0.1:9000".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn listen_addr_accepts_leading_colon() {
        assert_eq!(
            resolve_listen_addr(":9000").unwrap(),
            "127.0.0.1:9000".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn listen_addr_accepts_full_form() {
        assert_eq!(
            resolve_listen_addr("0.0.0.0:9000").unwrap(),
            "0.0.0.0:9000".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn listen_addr_rejects_garbage_instead_of_silently_defaulting() {
        assert!(resolve_listen_addr("not-an-address").is_err());
        assert!(resolve_listen_addr("").is_err());
        assert!(resolve_listen_addr("1.2.3").is_err());
    }
}
