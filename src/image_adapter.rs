//! Stability AI Stable Image (v2beta) image-generation adapter.
//!
//! Endpoint: `POST {base_url}/v2beta/stable-image/generate/{route}`
//! where `route` ∈ {`core`, `sd3`, `ultra`} (operator-selected via
//! the spec `model` field).
//!
//! Auth: `Authorization: Bearer <api_key>`.
//!
//! Wire format is **multipart/form-data** (NOT JSON). The
//! `Accept: application/json` request header asks Stability to
//! return a small JSON envelope with the image base64-encoded
//! inline:
//!
//! ```json
//! { "image": "<base64>", "seed": 12345, "finish_reason": "SUCCESS" }
//! ```
//!
//! `finish_reason: "CONTENT_FILTERED"` surfaces as
//! [`ProviderError::BadRequest`] so operators see *why* the call
//! came back with no bytes.
//!
//! Stability returns one image per call. The adapter rejects
//! `request.n > 1` with a clear `BadRequest`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use mcpg_backend_llm_shared::error::ProviderError;
use mcpg_backend_llm_shared::image::{
    GeneratedImage, ImageProviderAdapter, NormalizedImageRequest, NormalizedImageResponse,
};
use reqwest::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::Value;
use tracing::warn;

const TRACE_TARGET: &str = "mcpg::stability::image";

/// Aspect ratios Stability AI accepts on the `aspect_ratio` form
/// field. Listed widest-to-tallest. Operator-supplied `size` strings
/// are reduced to a `W:H` ratio and matched against this set.
pub(crate) const SUPPORTED_ASPECT_RATIOS: &[&str] = &[
    "21:9", "16:9", "3:2", "5:4", "1:1", "4:5", "2:3", "9:16", "9:21",
];

pub struct StabilityImageAdapter {
    client: Client,
    base_url: String,
    api_key: Arc<str>,
    /// One of `core` | `sd3` | `ultra` (lower-case). Validated at
    /// the spec layer before reaching the adapter.
    model_route: String,
}

impl std::fmt::Debug for StabilityImageAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StabilityImageAdapter")
            .field("base_url", &self.base_url)
            .field("model_route", &self.model_route)
            .finish()
    }
}

/// Stability's documented default when `output_format` is omitted —
/// also what we synthesize for response MIME tagging.
const DEFAULT_OUTPUT_FORMAT: &str = "png";

impl StabilityImageAdapter {
    /// Build a new adapter. `model_route` MUST already be one of
    /// `core` | `sd3` | `ultra` (the spec validates this); the
    /// constructor lowercases defensively but does not re-validate
    /// the membership.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model_route: impl Into<String>,
        connect_timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let client = Client::builder()
            .user_agent("mcpg-plugin-backend-llm-stability/1.0")
            .connect_timeout(connect_timeout)
            .build()
            .map_err(|e| ProviderError::Network {
                message: format!("build http client: {e}"),
            })?;
        let base_url = base_url.into();
        if base_url.is_empty() {
            return Err(ProviderError::BadRequest {
                message: "base_url is empty".into(),
            });
        }
        let model_route = model_route.into().trim().to_ascii_lowercase();
        if model_route.is_empty() {
            return Err(ProviderError::BadRequest {
                message: "model_route is empty".into(),
            });
        }
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key: Arc::from(api_key.into()),
            model_route,
        })
    }

    fn endpoint_url(&self) -> String {
        format!(
            "{}/v2beta/stable-image/generate/{}",
            self.base_url, self.model_route
        )
    }

    fn build_headers(&self) -> Result<HeaderMap, ProviderError> {
        let mut h = HeaderMap::new();
        h.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let key = self.api_key.as_ref();
        if key.is_empty() {
            return Err(ProviderError::AuthFailed {
                message: "api_key is empty".into(),
            });
        }
        let v = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| {
            ProviderError::BadRequest {
                message: "api_key contains characters not allowed in HTTP headers".into(),
            }
        })?;
        h.insert(AUTHORIZATION, v);
        Ok(h)
    }

    fn build_form(
        &self,
        request: &NormalizedImageRequest,
    ) -> Result<reqwest::multipart::Form, ProviderError> {
        if request.n > 1 {
            return Err(ProviderError::BadRequest {
                message: "Stability AI returns one image per call; pass n=1 (or omit)".into(),
            });
        }
        // `quality` and `style` have no Stability equivalents — log
        // a warn and ignore. The shared `NormalizedImageRequest`
        // surfaces them as `Option<String>` so we just notice when
        // they're non-None.
        if request.quality.is_some() {
            warn!(
                target: TRACE_TARGET,
                "stability.image: ignoring `quality` (no Stability equivalent)"
            );
        }
        if request.style.is_some() {
            warn!(
                target: TRACE_TARGET,
                "stability.image: ignoring `style` (no Stability equivalent)"
            );
        }

        let mut form = reqwest::multipart::Form::new().text("prompt", request.prompt.clone());

        if let Some(size) = request.size.as_deref() {
            let aspect = aspect_ratio_for(size)?;
            form = form.text("aspect_ratio", aspect);
        }

        let fmt = output_format_for(request).trim().to_ascii_lowercase();
        if !fmt.is_empty() {
            form = form.text("output_format", fmt);
        }

        if let Some(seed) = request.seed {
            form = form.text("seed", seed.to_string());
        }

        // Stability `core` / `sd3` accept `negative_prompt`; `ultra`
        // rejects it. Pass through on the model_route check rather
        // than letting Stability 400.
        if let Some(neg) = request.negative_prompt.as_deref() {
            if self.model_route == "ultra" {
                warn!(
                    target: TRACE_TARGET,
                    "stability.image: ignoring `negative_prompt` (unsupported on ultra route)"
                );
            } else if !neg.is_empty() {
                form = form.text("negative_prompt", neg.to_owned());
            }
        }

        Ok(form)
    }
}

#[async_trait]
impl ImageProviderAdapter for StabilityImageAdapter {
    fn label(&self) -> &'static str {
        "stability"
    }

    async fn generate(
        &self,
        request: &NormalizedImageRequest,
        timeout: Duration,
    ) -> Result<NormalizedImageResponse, ProviderError> {
        let form = self.build_form(request)?;
        let headers = self.build_headers()?;
        let url = self.endpoint_url();

        let resp = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(timeout)
            .multipart(form)
            .send()
            .await
            .map_err(|e| ProviderError::Network {
                message: format!("send: {e}"),
            })?;

        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| ProviderError::Network {
            message: format!("read body: {e}"),
        })?;

        if !status.is_success() {
            return Err(map_status_error(status, &bytes));
        }

        let value: Value =
            serde_json::from_slice(&bytes).map_err(|e| ProviderError::Malformed {
                message: format!("parse response json: {e}"),
            })?;
        decode_response(&value, output_format_for(request))
    }
}

/// Resolve the `output_format` Stability should use for this request.
/// `request.output_format` is the operator-driven slot
/// (`ImageDefaults.output_format` or per-call `args.output_format`).
/// Empty / `None` falls back to Stability's documented default
/// ([`DEFAULT_OUTPUT_FORMAT`]).
fn output_format_for(request: &NormalizedImageRequest) -> &str {
    match request.output_format.as_deref() {
        Some(s) if !s.trim().is_empty() => s,
        _ => DEFAULT_OUTPUT_FORMAT,
    }
}

/// Reduce a `WxH` size string (or already-aspect-ratio `W:H`) to one
/// of [`SUPPORTED_ASPECT_RATIOS`]. Returns
/// [`ProviderError::BadRequest`] (with the supported set listed)
/// when no exact match exists — never silently rounds.
///
/// Aspect-ratio inputs (`W:H`) are matched verbatim FIRST so values
/// like `"21:9"` (whose GCD-reduced form is `"7:3"`, which Stability
/// does not accept) pass through unchanged.
fn aspect_ratio_for(size: &str) -> Result<String, ProviderError> {
    let (w, h, was_ratio) = parse_dims(size).ok_or_else(|| ProviderError::BadRequest {
        message: format!(
            "size must be `WxH` or `W:H`; got {size:?}. \
                 Supported aspect ratios: {SUPPORTED_ASPECT_RATIOS:?}"
        ),
    })?;
    if w == 0 || h == 0 {
        return Err(ProviderError::BadRequest {
            message: format!("size dimensions must be > 0; got {size:?}"),
        });
    }
    if was_ratio {
        // Operator already supplied a ratio — match verbatim.
        let verbatim = format!("{w}:{h}");
        if SUPPORTED_ASPECT_RATIOS.contains(&verbatim.as_str()) {
            return Ok(verbatim);
        }
        return Err(ProviderError::BadRequest {
            message: format!(
                "aspect ratio {verbatim} is not supported by Stability AI. \
                 Supported aspect ratios: {SUPPORTED_ASPECT_RATIOS:?}"
            ),
        });
    }
    let g = gcd(w, h);
    let candidate = format!("{}:{}", w / g, h / g);
    if SUPPORTED_ASPECT_RATIOS.contains(&candidate.as_str()) {
        Ok(candidate)
    } else {
        Err(ProviderError::BadRequest {
            message: format!(
                "aspect ratio {candidate} (from size {size:?}) is not supported by Stability AI. \
                 Supported aspect ratios: {SUPPORTED_ASPECT_RATIOS:?}"
            ),
        })
    }
}

/// Returns `(w, h, was_ratio)` where `was_ratio` is true when the
/// input used a `:` separator (i.e. operator already provided a
/// ratio rather than a pixel size).
fn parse_dims(s: &str) -> Option<(u32, u32, bool)> {
    let trimmed = s.trim();
    if let Some((w, h)) = trimmed.split_once(':') {
        return Some((w.trim().parse().ok()?, h.trim().parse().ok()?, true));
    }
    let (w, h) = trimmed
        .split_once('x')
        .or_else(|| trimmed.split_once('X'))?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?, false))
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn decode_response(
    value: &Value,
    default_output_format: &str,
) -> Result<NormalizedImageResponse, ProviderError> {
    let finish_reason = value
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("SUCCESS");
    if finish_reason.eq_ignore_ascii_case("CONTENT_FILTERED") {
        return Err(ProviderError::BadRequest {
            message: "content filtered by provider".into(),
        });
    }

    let b64 =
        value
            .get("image")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProviderError::Malformed {
                message: "image response missing `image` (base64) field".into(),
            })?;

    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| ProviderError::Malformed {
            message: format!("decode image base64: {e}"),
        })?;

    let mime_type = mime_for_output_format(default_output_format).to_owned();

    Ok(NormalizedImageResponse {
        images: vec![GeneratedImage {
            bytes: bytes::Bytes::from(raw),
            mime_type,
            // Stability does not echo a revised prompt.
            revised_prompt: None,
        }],
    })
}

fn mime_for_output_format(fmt: &str) -> &'static str {
    match fmt.trim().to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => "image/jpeg",
        "webp" => "image/webp",
        // png is the documented default + the safe fallback.
        _ => "image/png",
    }
}

fn map_status_error(status: reqwest::StatusCode, body: &[u8]) -> ProviderError {
    let body_str = String::from_utf8_lossy(body).to_string();
    // Stability's error envelope is `{"errors": ["..."]}`. Pull the
    // first message out so the surfaced error is short and useful;
    // fall back to the raw body otherwise.
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("errors")
                .and_then(|e| e.as_array())
                .and_then(|arr| arr.first())
                .and_then(|first| first.as_str())
                .map(|s| s.to_owned())
        })
        .unwrap_or(body_str);

    match status.as_u16() {
        401 | 403 => ProviderError::AuthFailed { message },
        429 => ProviderError::RateLimited { message },
        400..=499 => ProviderError::BadRequest { message },
        500..=599 => ProviderError::Server { message },
        _ => ProviderError::Network {
            message: format!("unexpected status {status}: {message}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(prompt: &str) -> NormalizedImageRequest {
        NormalizedImageRequest {
            model: "core".into(),
            prompt: prompt.into(),
            n: 1,
            size: None,
            quality: None,
            style: None,
            seed: None,
            negative_prompt: None,
            output_format: None,
        }
    }

    fn adapter(route: &str) -> StabilityImageAdapter {
        StabilityImageAdapter::new(
            "https://api.stability.ai",
            "k",
            route,
            Duration::from_secs(1),
        )
        .unwrap()
    }

    /// Build a multipart Form, then assert it succeeded — the
    /// reqwest `Form` doesn't expose its parts after construction,
    /// so the test asserts at the call site rather than reading
    /// fields back. The detailed cases below check error paths,
    /// which DO surface through `Result`.
    #[test]
    fn encode_form_includes_required_prompt() {
        let a = adapter("core");
        // Prompt-only request should build cleanly.
        a.build_form(&req("a cat sitting on a chair")).unwrap();
    }

    #[test]
    fn encode_form_maps_aspect_ratio_from_size_wh() {
        let a = adapter("core");
        let mut r = req("x");
        r.size = Some("1920x1080".into()); // 16:9
        a.build_form(&r).unwrap();

        r.size = Some("1024x1024".into()); // 1:1
        a.build_form(&r).unwrap();

        r.size = Some("9:16".into()); // already an aspect ratio
        a.build_form(&r).unwrap();
    }

    #[test]
    fn encode_form_rejects_unsupported_aspect_ratio() {
        let a = adapter("core");
        let mut r = req("x");
        r.size = Some("1234x567".into());
        let err = a.build_form(&r).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not supported"), "{msg}");
        assert!(msg.contains("1:1"), "{msg}"); // expect supported list listed
    }

    #[test]
    fn encode_form_rejects_unparseable_size() {
        let a = adapter("core");
        let mut r = req("x");
        r.size = Some("garbage".into());
        let err = a.build_form(&r).unwrap_err();
        assert!(matches!(err, ProviderError::BadRequest { .. }));
    }

    #[test]
    fn encode_form_rejects_n_greater_than_one() {
        let a = adapter("core");
        let mut r = req("x");
        r.n = 3;
        let err = a.build_form(&r).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("one image per call"), "{msg}");
    }

    #[test]
    fn output_format_for_falls_back_to_png_when_unset() {
        let r = req("x");
        assert_eq!(output_format_for(&r), "png");
    }

    #[test]
    fn output_format_for_uses_request_value_when_set() {
        let mut r = req("x");
        r.output_format = Some("webp".into());
        assert_eq!(output_format_for(&r), "webp");
    }

    #[test]
    fn output_format_for_treats_empty_as_unset() {
        let mut r = req("x");
        r.output_format = Some("   ".into());
        assert_eq!(output_format_for(&r), "png");
    }

    #[test]
    fn encode_form_accepts_negative_prompt_on_core() {
        let a = adapter("core");
        let mut r = req("x");
        r.negative_prompt = Some("blurry, extra limbs".into());
        a.build_form(&r).unwrap();
    }

    #[test]
    fn encode_form_drops_negative_prompt_on_ultra() {
        let a = adapter("ultra");
        let mut r = req("x");
        r.negative_prompt = Some("blurry".into());
        // build_form returns Ok — the field is silently dropped
        // with a warn, since `ultra` rejects it server-side.
        a.build_form(&r).unwrap();
    }

    #[test]
    fn aspect_ratio_for_wh_simplifies_via_gcd() {
        assert_eq!(aspect_ratio_for("1920x1080").unwrap(), "16:9");
        assert_eq!(aspect_ratio_for("1024x1024").unwrap(), "1:1");
        assert_eq!(aspect_ratio_for("1080x1920").unwrap(), "9:16");
        assert_eq!(aspect_ratio_for("1500x1000").unwrap(), "3:2");
    }

    #[test]
    fn aspect_ratio_for_passes_through_already_ratio() {
        assert_eq!(aspect_ratio_for("21:9").unwrap(), "21:9");
        assert_eq!(aspect_ratio_for("9:21").unwrap(), "9:21");
    }

    #[test]
    fn aspect_ratio_for_rejects_zero_dim() {
        assert!(aspect_ratio_for("0x100").is_err());
        assert!(aspect_ratio_for("100x0").is_err());
    }

    #[test]
    fn decode_response_parses_b64_image() {
        let payload = b"hello image bytes";
        let b64 = base64::engine::general_purpose::STANDARD.encode(payload);
        let raw = json!({
            "image": b64,
            "seed": 42,
            "finish_reason": "SUCCESS"
        });
        let r = decode_response(&raw, "png").unwrap();
        assert_eq!(r.images.len(), 1);
        assert_eq!(r.images[0].bytes.as_ref(), payload);
        assert_eq!(r.images[0].mime_type, "image/png");
        assert!(r.images[0].revised_prompt.is_none());
    }

    #[test]
    fn decode_response_uses_jpeg_mime_when_format_is_jpeg() {
        let payload = b"x";
        let b64 = base64::engine::general_purpose::STANDARD.encode(payload);
        let raw = json!({"image": b64, "finish_reason": "SUCCESS"});
        let r = decode_response(&raw, "jpeg").unwrap();
        assert_eq!(r.images[0].mime_type, "image/jpeg");
    }

    #[test]
    fn decode_response_uses_webp_mime_when_format_is_webp() {
        let payload = b"x";
        let b64 = base64::engine::general_purpose::STANDARD.encode(payload);
        let raw = json!({"image": b64, "finish_reason": "SUCCESS"});
        let r = decode_response(&raw, "webp").unwrap();
        assert_eq!(r.images[0].mime_type, "image/webp");
    }

    #[test]
    fn decode_response_surfaces_content_filtered_as_bad_request() {
        let raw = json!({
            "image": "",
            "finish_reason": "CONTENT_FILTERED"
        });
        let err = decode_response(&raw, "png").unwrap_err();
        match err {
            ProviderError::BadRequest { message } => {
                assert!(message.contains("content filtered"), "{message}")
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn decode_response_rejects_missing_image_field() {
        let raw = json!({"finish_reason": "SUCCESS"});
        let err = decode_response(&raw, "png").unwrap_err();
        assert!(matches!(err, ProviderError::Malformed { .. }));
    }

    #[test]
    fn decode_response_rejects_undecodable_base64() {
        let raw = json!({"image": "!!!not-base64!!!", "finish_reason": "SUCCESS"});
        let err = decode_response(&raw, "png").unwrap_err();
        assert!(matches!(err, ProviderError::Malformed { .. }));
    }

    #[test]
    fn map_status_429_rate_limited() {
        let e = map_status_error(reqwest::StatusCode::from_u16(429).unwrap(), b"slow down");
        assert!(matches!(e, ProviderError::RateLimited { .. }));
    }

    #[test]
    fn map_status_401_auth_failed() {
        let e = map_status_error(reqwest::StatusCode::from_u16(401).unwrap(), b"bad key");
        assert!(matches!(e, ProviderError::AuthFailed { .. }));
    }

    #[test]
    fn map_status_extracts_first_error_from_envelope() {
        let body = br#"{"errors": ["prompt is too long", "second error"]}"#;
        let e = map_status_error(reqwest::StatusCode::from_u16(400).unwrap(), body);
        match e {
            ProviderError::BadRequest { message } => {
                assert_eq!(message, "prompt is too long");
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn map_status_falls_back_to_raw_body_when_no_errors_field() {
        let body = b"upstream meltdown";
        let e = map_status_error(reqwest::StatusCode::from_u16(500).unwrap(), body);
        match e {
            ProviderError::Server { message } => {
                assert_eq!(message, "upstream meltdown");
            }
            other => panic!("expected Server, got {other:?}"),
        }
    }

    #[test]
    fn endpoint_url_assembles_route_path_core() {
        let a = adapter("core");
        assert_eq!(
            a.endpoint_url(),
            "https://api.stability.ai/v2beta/stable-image/generate/core"
        );
    }

    #[test]
    fn endpoint_url_assembles_route_path_sd3() {
        let a = adapter("sd3");
        assert_eq!(
            a.endpoint_url(),
            "https://api.stability.ai/v2beta/stable-image/generate/sd3"
        );
    }

    #[test]
    fn endpoint_url_assembles_route_path_ultra() {
        let a = adapter("ultra");
        assert_eq!(
            a.endpoint_url(),
            "https://api.stability.ai/v2beta/stable-image/generate/ultra"
        );
    }

    #[test]
    fn endpoint_url_trims_trailing_slash_on_base_url() {
        let a = StabilityImageAdapter::new(
            "https://api.stability.ai/",
            "k",
            "core",
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            a.endpoint_url(),
            "https://api.stability.ai/v2beta/stable-image/generate/core"
        );
    }

    #[test]
    fn build_headers_rejects_empty_api_key() {
        let a = StabilityImageAdapter::new(
            "https://api.stability.ai",
            "",
            "core",
            Duration::from_secs(1),
        )
        .unwrap();
        let err = a.build_headers().unwrap_err();
        assert!(matches!(err, ProviderError::AuthFailed { .. }));
    }
}
