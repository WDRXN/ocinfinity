use serde::{Deserialize, Serialize};

/// Runtime configuration for the Anthropic <-> OpenCode Zen bridge.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Address to listen on (default: 127.0.0.1:8080)
    pub listen_addr: String,
    /// OpenCode inference gateway base URL
    pub opencode_base_url: String,
    /// Gateway API key. OpenCode publishes `public` for its own gateway.
    pub opencode_api_key: Option<String>,
    /// Maximum retry attempts after WARP reset
    pub max_retries: u32,
    /// Delay between WARP reset steps (milliseconds)
    pub warp_reset_delay_ms: u64,
}

/// A free model available on OpenCode Zen.
#[derive(Debug, Clone, Serialize)]
pub struct FreeModel {
    /// Model identifier used by OpenCode Zen
    pub id: String,
    /// Display name shown in Claude Code
    pub display_name: String,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1:8080".to_string(),
            opencode_base_url: crate::detect::DEFAULT_OPENCODE_URL.to_string(),
            opencode_api_key: Some(crate::detect::DEFAULT_OPENCODE_API_KEY.to_string()),
            max_retries: 3,
            warp_reset_delay_ms: 5000,
        }
    }
}

impl ProxyConfig {
    /// Returns the free tier of the gateway, used when it cannot be listed.
    ///
    /// This is the fallback catalogue shown when OpenCode Zen is unreachable;
    /// `/v1/models` always prefers the live upstream list when it answers.
    pub fn free_models() -> Vec<FreeModel> {
        vec![
            ("big-pickle", "Big Pickle"),
            ("glm-5.3-flash-free", "GLM 5.3 Flash Free"),
            ("deepseek-v4.1-flash-free", "DeepSeek V4.1 Flash Free"),
            ("mimo-v2.6-flash-free", "MiMo V2.6 Flash Free"),
            ("nemotron-3.5-lightning-free", "Nemotron 3.5 Lightning Free"),
            ("nemotron-3.5-super-vl-free", "Nemotron 3.5 Super VL Free"),
            ("nemotron-3-ultra-free", "Nemotron 3 Ultra Free"),
            ("beam-free", "Beam Free"),
            ("flint-40b-free", "Flint 40B Free"),
            ("jev-1.13-free", "Jev 1.13 Free"),
        ]
        .into_iter()
        .map(|(id, display_name)| FreeModel {
            id: id.to_string(),
            display_name: display_name.to_string(),
        })
        .collect()
    }

    /// Build the fallback models response in OpenAI list shape.
    pub fn models_response() -> serde_json::Value {
        let models: Vec<serde_json::Value> = Self::free_models()
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "object": "model",
                    "created": 0,
                    "owned_by": "opencode-zen"
                })
            })
            .collect();

        serde_json::json!({
            "object": "list",
            "data": models
        })
    }
}

/// Persisted user configuration, backing `ocinfinity config show|set|reset`.
///
/// Every field is optional on disk so a config written by an older release
/// still loads after an upgrade (`#[serde(default)]` plus per-field defaults).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub listen: String,
    pub upstream: String,
    pub api_key: Option<String>,
    pub max_retries: u32,
    pub warp_delay: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".to_string(),
            upstream: crate::detect::DEFAULT_OPENCODE_URL.to_string(),
            api_key: Some(crate::detect::DEFAULT_OPENCODE_API_KEY.to_string()),
            max_retries: 3,
            warp_delay: 5000,
        }
    }
}

impl AppConfig {
    /// Overlay this saved configuration onto a `ProxyConfig` skeleton, filling
    /// in anything the saved file does not mention.
    pub fn to_proxy_config(&self) -> ProxyConfig {
        ProxyConfig {
            listen_addr: self.listen.clone(),
            opencode_base_url: self.upstream.clone(),
            opencode_api_key: self
                .api_key
                .clone()
                .or_else(|| Some(crate::detect::DEFAULT_OPENCODE_API_KEY.to_string())),
            max_retries: self.max_retries,
            warp_reset_delay_ms: self.warp_delay,
        }
    }
}
