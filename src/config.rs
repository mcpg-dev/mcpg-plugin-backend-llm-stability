//! Operator-facing config for the Stability AI Stable Image binding.
//!
//! Same flatten-passthrough pattern as the OpenAI / Gemini image
//! specs — provider-specific knobs (`api_key`, optional `base_url`
//! override) plus an embedded [`ImageExecutionSpec`] for the
//! provider-agnostic surface (`model`, `timeout_ms`, defaults,
//! retry).
//!
//! The `model` field carries one of Stability's three SKUs —
//! `"core"`, `"sd3"`, or `"ultra"` — which selects the
//! `/v2beta/stable-image/generate/<route>` endpoint. The plugin
//! validates the value at register time so a typo fails fast with a
//! clear error.

use mcpg_backend_llm_shared::{ApiKeyRef, ConfigError, ImageExecutionSpec};
use serde::{Deserialize, Serialize};

/// Spec for `binding_type: stability_image`.
///
/// Default `base_url`: `https://api.stability.ai`. Operators can
/// override (e.g. to point at a forwarding proxy or test fixture).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StabilityImageSpec {
    /// Override for the default `https://api.stability.ai`. Most
    /// operators leave this unset.
    #[serde(default)]
    pub base_url: Option<String>,

    pub api_key: ApiKeyRef,

    /// Provider-agnostic execution config. The `model` field MUST
    /// be one of `core` | `sd3` | `ultra` (case-insensitive); the
    /// adapter uses it as the `/v2beta/stable-image/generate/<x>`
    /// route segment.
    #[serde(flatten)]
    pub image: ImageExecutionSpec,
}

impl StabilityImageSpec {
    pub const DEFAULT_BASE_URL: &'static str = "https://api.stability.ai";

    /// Stability AI's three exposed SKUs, one of which `image.model`
    /// must equal (case-insensitive).
    pub const SUPPORTED_MODELS: &'static [&'static str] = &["core", "sd3", "ultra"];

    pub fn validate(&self) -> Result<(), ConfigError> {
        // Run the provider-agnostic validation first (non-empty
        // model, positive timeouts, …) so its error wins on truly
        // empty configs.
        self.image.validate()?;

        let normalized = self.image.model.trim().to_ascii_lowercase();
        if !Self::SUPPORTED_MODELS.contains(&normalized.as_str()) {
            return Err(ConfigError::InvalidSpec(format!(
                "stability_image: model must be one of {:?}, got {:?}",
                Self::SUPPORTED_MODELS,
                self.image.model
            )));
        }
        Ok(())
    }

    /// Resolve the base URL with the Stability default applied when
    /// the operator hasn't supplied an override.
    pub fn resolved_base_url(&self) -> &str {
        self.base_url.as_deref().unwrap_or(Self::DEFAULT_BASE_URL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn minimal_image_exec(model: &str) -> ImageExecutionSpec {
        ImageExecutionSpec {
            model: model.into(),
            ..Default::default()
        }
    }

    #[test]
    fn default_base_url() {
        let s = StabilityImageSpec {
            base_url: None,
            api_key: ApiKeyRef::new("k"),
            image: minimal_image_exec("core"),
        };
        assert_eq!(s.resolved_base_url(), "https://api.stability.ai");
        s.validate().unwrap();
    }

    #[test]
    fn override_base_url() {
        let s = StabilityImageSpec {
            base_url: Some("https://example.com".into()),
            api_key: ApiKeyRef::new("k"),
            image: minimal_image_exec("ultra"),
        };
        assert_eq!(s.resolved_base_url(), "https://example.com");
    }

    #[test]
    fn each_supported_model_validates() {
        for m in ["core", "sd3", "ultra", "CORE", "Sd3"] {
            let s = StabilityImageSpec {
                base_url: None,
                api_key: ApiKeyRef::new("k"),
                image: minimal_image_exec(m),
            };
            s.validate().expect(m);
        }
    }

    #[test]
    fn unknown_model_rejected() {
        let s = StabilityImageSpec {
            base_url: None,
            api_key: ApiKeyRef::new("k"),
            image: minimal_image_exec("dall-e-3"),
        };
        let err = s.validate().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("core"));
        assert!(msg.contains("sd3"));
        assert!(msg.contains("ultra"));
    }

    #[test]
    fn json_round_trip() {
        let v = json!({
            "model": "core",
            "api_key": "k"
        });
        let s: StabilityImageSpec = serde_json::from_value(v).unwrap();
        s.validate().unwrap();
        assert_eq!(s.image.model, "core");
        assert!(s.base_url.is_none());
    }
}
