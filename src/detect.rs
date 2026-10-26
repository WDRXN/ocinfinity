//! Automatic service and port discovery.
//!
//! `ocinfinity` has to work without the operator telling it where OpenCode Zen
//! is listening, or which port is safe for the local proxy. Everything in this
//! module is blocking and uses very short timeouts, so discovery can run before
//! a command starts without adding noticeable latency.
//!
//! Detection order for the upstream:
//! 1. An explicitly supplied `--upstream` value (never overridden).
//! 2. A saved `upstream` in the config file, if it currently answers.
//! 3. The inference gateway configured in the local OpenCode v2 service
//!    (`GET /api/provider`), which is authoritative even though v2 picks an
//!    ephemeral port at every start.
//! 4. The first candidate port that answers `GET /v1/models` and is *not* one
//!    of our own proxy instances.
//! 5. [`DEFAULT_OPENCODE_URL`] as a last resort.
//!
//! OpenCode v2's local server speaks `/api/*` only — it has no OpenAI-shaped
//! chat endpoint, so it can never be the upstream itself. What it *does* expose
//! is the provider it uses for inference, and that provider is plain
//! OpenAI-compatible. Discovery therefore reads the provider out of the service
//! instead of trying to talk to the service directly.

use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use crate::platform;
use crate::proxy::server::INSTANCE_HEADER;

/// OpenCode inference gateway used when nothing else is detected.
///
/// Serves both `GET /models` and `POST /chat/completions`. The `openai`
/// qualified variant the v2 client uses internally
/// (`https://opencode.ai/inference/openai/v1`) answers chat only, so the
/// unqualified path is the one that can also list models.
pub const DEFAULT_OPENCODE_URL: &str = "https://opencode.ai/inference/v1";

/// Key the OpenCode v2 client sends to its own gateway. It is intentionally
/// public: the gateway authorises by account/IP, not by secret.
pub const DEFAULT_OPENCODE_API_KEY: &str = "public";

/// Identification sent on every outbound request.
///
/// Cloudflare sits in front of the gateway and rejects requests that carry no
/// user agent at all, so this is a hard requirement rather than politeness.
pub const USER_AGENT: &str = concat!("ocinfinity/", env!("CARGO_PKG_VERSION"));

/// Marker our own proxy puts in `/v1/models` (`owned_by`). Used as a fallback
/// in case a response is proxied through something that strips headers.
const SELF_MARKER: &str = "\"opencode-zen\"";

const PROBE_TIMEOUT: Duration = Duration::from_millis(700);

/// Local servers that answer `/v1/models` but are never OpenCode Zen.
const KNOWN_FOREIGN_PORTS: [u16; 2] = [11434, 7860];

/// A service found during discovery.
#[derive(Debug, Clone)]
pub struct DetectedService {
    pub name: &'static str,
    pub url: String,
    pub port: u16,
    pub detail: String,
}

/// Result of probing one port.
struct Probe {
    body: String,
    /// True when the responder identified itself as an `ocinfinity` proxy.
    ours: bool,
}

/// Ports scanned during automatic discovery, most likely first.
///
/// OpenCode's own ports come first, then the ports `ocinfinity` itself
/// defaults to, then general-purpose local servers. `11434` (Ollama) and
/// `7860` (Gradio) are last because they answer `/v1/models` too but are not
/// OpenCode.
pub fn candidate_ports() -> Vec<u16> {
    vec![
        3000, 4096, 8080, 8081, 3001, 8000, 8888, 5000, 7777, 9000, 1234, 11434, 7860,
    ]
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .unwrap_or_default()
}

/// Join a base URL and an API path without duplicating the `/v1` segment.
///
/// Providers publish their base either with or without the version suffix
/// (`https://opencode.ai/inference/v1` vs `.../openai/v1`), and appending
/// `/v1/models` blindly produced `/v1/v1/models` — a 404 on every real
/// deployment.
pub fn endpoint(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let path = path.trim_start_matches('/');

    let path = match path.strip_prefix("v1/") {
        Some(rest) if base.ends_with("/v1") => rest,
        _ => path,
    };

    format!("{}/{}", base, path)
}

/// Fold the version-qualified gateway URL onto the one that also lists models.
///
/// The v2 client stores `https://opencode.ai/inference/openai/v1`, which serves
/// chat but answers 404 for `/models`. Dropping the vendor segment keeps chat
/// working and makes model discovery work as well.
pub fn normalize_base(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    match trimmed.strip_suffix("/openai/v1") {
        Some(prefix) => format!("{}/v1", prefix),
        None => trimmed.to_string(),
    }
}

/// `GET /v1/models` on `port`, if it answers successfully.
fn models_probe(port: u16) -> Option<Probe> {
    let url = endpoint(&format!("http://127.0.0.1:{}", port), "/v1/models");
    let response = client().get(&url).send().ok()?;
    if !response.status().is_success() {
        return None;
    }

    let ours = response.headers().contains_key(INSTANCE_HEADER);
    let body = response.text().ok()?;
    Some(Probe { body, ours })
}

/// Probe every candidate port in parallel and return all that answered.
///
/// Probes run on their own threads so total latency is one timeout window
/// rather than one per port.
fn probe_all() -> Vec<(u16, Probe)> {
    let handles: Vec<_> = candidate_ports()
        .into_iter()
        .map(|port| std::thread::spawn(move || (port, models_probe(port))))
        .collect();

    let mut found = Vec::new();
    for handle in handles {
        if let Ok((port, Some(probe))) = handle.join() {
            found.push((port, probe));
        }
    }

    // Lowest candidate port first, so 3000 wins over 8080 when both answer.
    found.sort_by_key(|(port, _)| *port);
    found
}

/// Whether a probe belongs to one of our own proxies.
fn is_self(probe: &Probe) -> bool {
    probe.ours || probe.body.contains(SELF_MARKER)
}

/// How strongly a `/v1/models` payload looks like OpenCode Zen.
///
/// OpenCode Zen model ids carry `opencode` in `owned_by` or a `-free` suffix.
/// Ollama and other local servers answer the same endpoint with a score of
/// zero, so they only win when nothing else is listening.
fn opencode_score(body: &str) -> u32 {
    let lower = body.to_ascii_lowercase();
    let mut score = 0;
    if lower.contains("opencode") {
        score += 4;
    }
    if lower.contains("-free") {
        score += 2;
    }
    if lower.contains("\"owned_by\"") {
        score += 1;
    }
    score
}

/// Path of the OpenCode v2 background-service state file.
///
/// v2 keeps the same `~/.config/opencode` layout on every platform, including
/// Windows, which is why this does not use a per-OS directory.
fn v2_service_state_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".config").join("opencode").join("service.json"))
}

/// Password guarding the local OpenCode v2 service.
fn v2_service_password() -> Option<String> {
    let raw = std::fs::read_to_string(v2_service_state_path()?).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("password")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|p| !p.is_empty())
}

/// Base URL of the running OpenCode v2 background service.
///
/// v2 binds an ephemeral port on every start, so the port cannot be guessed;
/// the CLI itself reports it. `opencode` is resolved against `PATH` (and
/// `PATHEXT` on Windows) because the installer ships a `.cmd` shim.
pub fn v2_service_url() -> Option<String> {
    let exe = platform::which("opencode").unwrap_or_else(|| PathBuf::from("opencode"));

    let output = platform::command_output(&exe, &["service", "status"])?;
    let text = String::from_utf8_lossy(&output).to_string();

    text.split_whitespace()
        .find(|token| token.starts_with("http://") || token.starts_with("https://"))
        .map(|token| token.trim_end_matches('/').to_string())
}

/// Provider the local OpenCode v2 service uses for inference.
///
/// Returns `(base_url, api_key)` for the provider with id `opencode`. This is
/// the authoritative upstream on a v2 install: it tracks whatever OpenCode
/// itself is configured to call, including account-specific gateways.
pub fn v2_gateway() -> Option<(String, Option<String>)> {
    let service = v2_service_url()?;
    let password = v2_service_password()?;

    let response = client()
        .get(format!("{}/api/provider", service))
        .basic_auth("opencode", Some(password))
        .header("Accept", "application/json")
        .send()
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let payload: serde_json::Value = response.json().ok()?;
    let providers = payload.get("data")?.as_array()?;

    let provider = providers
        .iter()
        .find(|p| p.get("id").and_then(serde_json::Value::as_str) == Some("opencode"))?;

    let settings = provider.get("settings")?;
    let base = settings
        .get("baseURL")
        .and_then(serde_json::Value::as_str)
        .map(normalize_base)?;

    let key = settings
        .get("apiKey")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|k| !k.is_empty());

    Some((base, key))
}

/// Discover the OpenCode Zen upstream.
///
/// v2 resolves first because it is authoritative and does not depend on a
/// scannable port. Candidates found by scanning are ranked by
/// [`opencode_score`] first and port order second, so a real OpenCode instance
/// always beats an unrelated OpenAI-compatible server.
pub fn detect_opencode() -> Option<DetectedService> {
    if let Some((base, _)) = v2_gateway() {
        let models = client()
            .get(endpoint(&base, "/v1/models"))
            .send()
            .ok()
            .filter(|r| r.status().is_success())
            .and_then(|r| r.text().ok())
            .map(|body| count_models(&body))
            .unwrap_or(0);

        let detail = if models > 0 {
            format!("{} model(s) via the OpenCode v2 gateway", models)
        } else {
            "OpenCode v2 gateway".to_string()
        };

        return Some(DetectedService {
            name: "OpenCode",
            url: base,
            port: 0,
            detail,
        });
    }

    let mut best: Option<(u32, u16, String)> = None;

    for (port, probe) in probe_all() {
        // Skip our own proxy so we never point the upstream at ourselves.
        if is_self(&probe) {
            continue;
        }

        // Anything that answers `/v1/models` with JSON is a usable upstream.
        let body = probe.body.trim_start();
        if !body.starts_with('{') && !body.starts_with('[') {
            continue;
        }

        let score = opencode_score(&probe.body);

        // Ollama and Gradio answer this endpoint too. Ignore them unless the
        // payload explicitly mentions OpenCode.
        if KNOWN_FOREIGN_PORTS.contains(&port) && score < 4 {
            continue;
        }

        let better = match &best {
            None => true,
            Some((best_score, best_port, _)) => {
                score > *best_score || (score == *best_score && port < *best_port)
            }
        };
        if better {
            best = Some((score, port, probe.body));
        }
    }

    let (score, port, body) = best?;
    let models = count_models(&body);

    Some(DetectedService {
        name: "OpenCode",
        url: format!("http://localhost:{}", port),
        port,
        detail: if score >= 2 {
            format!("{} model(s) via /v1/models", models)
        } else {
            // Reachable, but nothing identifies it as OpenCode Zen.
            format!("{} model(s), unidentified OpenAI-compatible server", models)
        },
    })
}

/// Locate an already-running `ocinfinity` proxy instance.
pub fn detect_running_proxy() -> Option<DetectedService> {
    for (port, probe) in probe_all() {
        if is_self(&probe) {
            return Some(DetectedService {
                name: "ocinfinity proxy",
                url: format!("http://localhost:{}", port),
                port,
                detail: "already bridging Anthropic traffic".to_string(),
            });
        }
    }

    None
}

/// Best-effort parse of the number of entries in a `/v1/models` payload.
fn count_models(body: &str) -> usize {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("data").and_then(|d| d.as_array()).map(|a| a.len()))
        .unwrap_or(0)
}

/// Locate the Claude Code CLI on `PATH`.
pub fn detect_claude_code() -> Option<PathBuf> {
    platform::which("claude")
}

/// Version string reported by the installed Claude Code CLI.
pub fn claude_version() -> Option<String> {
    let path = detect_claude_code()?;
    let output = std::process::Command::new(path)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Whether a TCP port on loopback can be bound right now.
pub fn port_is_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Pick a usable loopback port, starting at `preferred`.
///
/// Falls back to the first free port in `preferred..preferred + 50`, and
/// finally to `preferred` so the caller still gets a deterministic value.
pub fn free_port(preferred: u16) -> u16 {
    if port_is_free(preferred) {
        return preferred;
    }
    for offset in 1..50u16 {
        let candidate = preferred.saturating_add(offset);
        if port_is_free(candidate) {
            return candidate;
        }
    }
    preferred
}

/// Resolve the upstream to use for this run.
///
/// An explicit value always wins. Otherwise the saved config value is used when
/// it currently answers, then live discovery, then the conventional default.
pub fn resolve_upstream(explicit: Option<&str>, saved: Option<&str>) -> (String, &'static str) {
    if let Some(value) = explicit {
        if !value.trim().is_empty() {
            return (value.trim().to_string(), "argument");
        }
    }

    if let Some(saved) = saved {
        if !saved.trim().is_empty() && probe_url(saved) {
            return (saved.trim().to_string(), "config");
        }
    }

    if let Some(service) = detect_opencode() {
        return (service.url, "auto-detect");
    }

    (DEFAULT_OPENCODE_URL.to_string(), "default")
}

/// Whether `base_url` answers `GET /v1/models`.
pub fn probe_url(base_url: &str) -> bool {
    let url = endpoint(base_url, "/v1/models");
    client()
        .get(&url)
        .header(
            "Authorization",
            format!("Bearer {}", DEFAULT_OPENCODE_API_KEY),
        )
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Full discovery snapshot used by `ocinfinity detect` and `doctor`.
pub fn snapshot() -> Vec<DetectedService> {
    let mut services = Vec::new();

    if let Some(opencode) = detect_opencode() {
        services.push(opencode);
    }

    // The v2 background service is not itself an upstream (it only speaks
    // `/api/*`), but it is what supplies the gateway above, so report it.
    if let Some(url) = v2_service_url() {
        let port = url
            .rsplit(':')
            .next()
            .and_then(|p| p.trim_end_matches('/').parse::<u16>().ok())
            .unwrap_or(0);

        let detail = match v2_gateway() {
            Some((base, _)) => format!("background service, gateway {}", base),
            None => "background service".to_string(),
        };

        services.push(DetectedService {
            name: "OpenCode v2",
            url,
            port,
            detail,
        });
    }

    if let Some(proxy) = detect_running_proxy() {
        services.push(proxy);
    }
    if let Some(path) = detect_claude_code() {
        services.push(DetectedService {
            name: "Claude Code",
            url: path.display().to_string(),
            port: 0,
            detail: claude_version().unwrap_or_else(|| "installed".to_string()),
        });
    }
    if platform::is_warp_installed() {
        services.push(DetectedService {
            name: "Cloudflare WARP",
            url: String::new(),
            port: 0,
            detail: "installed".to_string(),
        });
    }

    services
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_ports_cover_opencode_and_proxy_defaults() {
        let ports = candidate_ports();
        assert!(ports.contains(&3000), "OpenCode's conventional port");
        assert!(ports.contains(&8080), "ocinfinity's own default");
        assert_eq!(
            ports.len(),
            ports.iter().collect::<std::collections::HashSet<_>>().len()
        );
    }

    #[test]
    fn explicit_upstream_always_wins() {
        let (url, source) =
            resolve_upstream(Some("http://example.test:1234"), Some("http://other"));
        assert_eq!(url, "http://example.test:1234");
        assert_eq!(source, "argument");
    }

    #[test]
    fn blank_explicit_upstream_is_ignored() {
        let (_, source) = resolve_upstream(Some("   "), Some("http://127.0.0.1:1"));
        assert_ne!(source, "argument");
    }

    #[test]
    fn free_port_returns_a_bindable_port() {
        let port = free_port(45_000);
        assert!(port_is_free(port) || port == 45_000);
    }

    #[test]
    fn self_marker_excludes_our_own_proxy() {
        let probe = Probe {
            body: r#"{"object":"list","data":[]}"#.to_string(),
            ours: true,
        };
        assert!(is_self(&probe));

        let foreign = Probe {
            body: r#"{"object":"list","data":[]}"#.to_string(),
            ours: false,
        };
        assert!(!is_self(&foreign));
    }

    #[test]
    fn endpoint_does_not_duplicate_the_version_segment() {
        assert_eq!(
            endpoint("https://opencode.ai/inference/v1", "/v1/models"),
            "https://opencode.ai/inference/v1/models"
        );
        assert_eq!(
            endpoint("http://localhost:4096", "/v1/models"),
            "http://localhost:4096/v1/models"
        );
        assert_eq!(
            endpoint("http://localhost:4096/", "/v1/chat/completions"),
            "http://localhost:4096/v1/chat/completions"
        );
    }

    #[test]
    fn normalize_base_drops_the_vendor_segment() {
        assert_eq!(
            normalize_base("https://opencode.ai/inference/openai/v1"),
            "https://opencode.ai/inference/v1"
        );
        assert_eq!(
            normalize_base("https://opencode.ai/inference/v1"),
            "https://opencode.ai/inference/v1"
        );
        assert_eq!(
            normalize_base("http://localhost:4096/"),
            "http://localhost:4096"
        );
    }

    #[test]
    fn count_models_reads_the_data_array() {
        let body = r#"{"object":"list","data":[{"id":"a"},{"id":"b"}]}"#;
        assert_eq!(count_models(body), 2);
        assert_eq!(count_models("not json"), 0);
    }

    #[test]
    fn opencode_shaped_payloads_outscore_generic_servers() {
        let opencode = r#"{"data":[{"id":"glm-4.7-free"}],"owned_by":"opencode-zen"}"#;
        let ollama = r#"{"data":[{"id":"llama3:latest","owned_by":"library"}]}"#;
        assert!(opencode_score(opencode) > opencode_score(ollama));
        assert!(opencode_score(ollama) < opencode_score(r#"{"data":[{"id":"x-free"}]}"#));
    }
}
