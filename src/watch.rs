use std::time::Duration;
use tokio::time::interval;
use tracing::{error, info, warn};

use crate::warp::WarpResolver;

/// Cheapest model used for the one-token rate-limit probe.
const PROBE_MODEL: &str = "big-pickle";

/// Poll OpenCode Zen and rotate the WARP identity whenever it reports 429.
///
/// This is a background companion to the proxy: the proxy reacts to rate
/// limits on the request that hit them, whereas watch mode keeps probing so a
/// tunnel that goes stale while idle still gets recovered.
pub async fn start_watch_mode(
    upstream: &str,
    api_key: &Option<String>,
    max_retries: u32,
    warp_delay_ms: u64,
) -> anyhow::Result<()> {
    let client = crate::proxy::server::build_client()?;

    let resolver = WarpResolver::new(max_retries, warp_delay_ms);

    // The rate limit is enforced on inference, not on model listing: `/models`
    // answers 200 even when the account is throttled. Probing it would report a
    // healthy tunnel forever, so the probe is a one-token chat request.
    let probe_url = crate::detect::endpoint(upstream, "/v1/chat/completions");
    let probe_body = serde_json::json!({
        "model": PROBE_MODEL,
        "messages": [{ "role": "user", "content": "ping" }],
        "max_tokens": 1,
    });

    let probe = || {
        let mut request = client
            .post(&probe_url)
            .header("Content-Type", "application/json")
            .json(&probe_body);
        if let Some(key) = api_key.as_ref().filter(|k| !k.is_empty()) {
            request = request.bearer_auth(key);
        }
        request
    };

    let mut check_interval = interval(Duration::from_secs(30));
    let mut consecutive_429 = 0u32;

    info!("watch mode active; probing {} every 30s", probe_url);
    info!("press Ctrl+C to stop");

    loop {
        tokio::select! {
            _ = check_interval.tick() => {}
            _ = tokio::signal::ctrl_c() => {
                info!("watch mode stopped");
                return Ok(());
            }
        }

        match probe().send().await {
            Ok(resp) => {
                let status = resp.status();

                if status.is_success() {
                    if consecutive_429 > 0 {
                        info!(
                            "rate limit recovered after {} consecutive 429s",
                            consecutive_429
                        );
                    }
                    consecutive_429 = 0;
                    info!("OpenCode Zen: OK ({})", status);
                } else if status.as_u16() == 429 {
                    consecutive_429 += 1;
                    warn!(
                        "rate limit detected ({} consecutive); rotating WARP",
                        consecutive_429
                    );

                    if resolver.handle_429(consecutive_429 - 1).await {
                        tokio::time::sleep(Duration::from_secs(5)).await;

                        match probe().send().await {
                            Ok(recheck) if recheck.status().is_success() => {
                                info!("recovery confirmed");
                                consecutive_429 = 0;
                            }
                            Ok(recheck) => {
                                warn!("recovery check still failing ({})", recheck.status());
                            }
                            Err(e) => warn!("recovery check failed: {}", e),
                        }
                    } else {
                        error!("WARP reset failed; rate limit persists");
                    }
                } else {
                    warn!("OpenCode Zen returned unexpected status: {}", status);
                }
            }
            Err(e) => {
                // A transport error is not a rate limit; do not rotate on it.
                error!("failed to reach OpenCode Zen at {}: {}", upstream, e);
            }
        }
    }
}
