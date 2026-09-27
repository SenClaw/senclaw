//! Vision capability resolution.
//!
//! Port of TS `util/vision.ts`.

use super::ModelProfile;
use once_cell::sync::Lazy;
use regex::Regex;

/// Vision patterns for model name matching.
static VISION_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        // OpenAI
        Regex::new(r"(?i)^gpt-4o").unwrap(),
        Regex::new(r"(?i)^gpt-4(\.\d+)?-vision").unwrap(),
        Regex::new(r"(?i)^gpt-[5-9]").unwrap(),
        // o-series reasoning models (o1, o3, o4-mini, …) — all vision-capable.
        Regex::new(r"(?i)^o[1-9]").unwrap(),
        Regex::new(r"(?i)^chatgpt-4o").unwrap(),
        // Anthropic — every Claude 3 and newer sees images. Matched unanchored
        // so gateway-prefixed ids (`anthropic/…`, `openrouter/anthropic/…`) hit
        // too, and open-ended on the generation digit: pinning it to the
        // generations that existed when this was written silently demoted each
        // new release to the OCR fallback.
        Regex::new(r"(?i)claude-[3-9]").unwrap(),
        Regex::new(r"(?i)claude-(opus|sonnet|haiku|fable)-[3-9]").unwrap(),
        // Qwen-VL 系列
        Regex::new(r"(?i)qwen.*-vl").unwrap(),
        Regex::new(r"(?i)qwen2(\.\d+)?-vl").unwrap(),
        Regex::new(r"(?i)qwen3(\.\d+)?-plus").unwrap(),
        Regex::new(r"(?i)qvq").unwrap(),
        // Moonshot Kimi vision
        Regex::new(r"(?i)moonshot-v1-.*-vision").unwrap(),
        Regex::new(r"(?i)kimi.*vision").unwrap(),
        Regex::new(r"(?i)^kimi-k2\.6").unwrap(),
        Regex::new(r"(?i)kimi-latest").unwrap(),
        // GLM-4V / Zhipu
        Regex::new(r"(?i)glm-4v").unwrap(),
        Regex::new(r"(?i)glm-4\.\d+v").unwrap(),
        // Google Gemini（经 OpenRouter 接入时）
        Regex::new(r"(?i)gemini.*pro").unwrap(),
        Regex::new(r"(?i)gemini.*flash").unwrap(),
        Regex::new(r"(?i)gemini-1\.5").unwrap(),
        Regex::new(r"(?i)gemini-[2-9]").unwrap(),
        // DeepSeek-VL
        Regex::new(r"(?i)deepseek-vl").unwrap(),
        // Llama 3.2 vision
        Regex::new(r"(?i)llama-3\.2.*vision").unwrap(),
        // 通用关键字
        Regex::new(r"(?i)-vl-").unwrap(),
        Regex::new(r"(?i)-vision").unwrap(),
        Regex::new(r"(?i)-vlm").unwrap(),
    ]
});

/// Infer vision capability from model name.
///
/// Returns true if the model name matches known vision patterns.
pub fn infer_vision(model_name: &str) -> bool {
    if model_name.is_empty() {
        return false;
    }
    VISION_PATTERNS.iter().any(|re| re.is_match(model_name))
}

/// Determine if a model has vision capability.
///
/// Explicit vision field takes priority; if not declared, infer from model name.
pub fn model_has_vision(profile: &ModelProfile) -> bool {
    if let Some(vision) = profile.vision {
        return vision;
    }
    infer_vision(&profile.model_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_vision_patterns() {
        assert!(infer_vision("gpt-4o"));
        assert!(infer_vision("gpt-4-vision"));
        assert!(infer_vision("claude-3-5-sonnet-20241022"));
        assert!(infer_vision("claude-opus-4-7"));
        assert!(infer_vision("qwen-vl-max"));
        assert!(infer_vision("qwen2.5-vl-72b-instruct"));
        assert!(infer_vision("moonshot-v1-8k-vision-preview"));
        assert!(infer_vision("glm-4v-plus"));
        assert!(infer_vision("deepseek-vl2"));
        assert!(!infer_vision("gpt-3.5-turbo"));
        assert!(!infer_vision("deepseek-chat"));
        assert!(!infer_vision("qwen-plus"));
    }

    /// The routing this feeds is now load-bearing: a miss here doesn't degrade
    /// the answer politely, it sends the turn down the OCR path and the user
    /// never learns their vision model was ignored.
    #[test]
    fn test_infer_vision_current_generations() {
        for name in [
            "claude-sonnet-4-5",
            "claude-opus-5",
            "claude-fable-5",
            "anthropic/claude-sonnet-4.5",
            "openrouter/anthropic/claude-opus-5",
            "gpt-5",
            "gpt-5.1-mini",
            "o3-mini",
            "o4-mini",
            "gemini-3-pro",
            "gemini-2.5-flash",
        ] {
            assert!(infer_vision(name), "{name} should be recognized as vision");
        }
        // Claude 2 predates image input; nothing here should widen to it.
        assert!(!infer_vision("claude-2.1"));
        assert!(!infer_vision("claude-instant-1.2"));
    }

    #[test]
    fn test_model_has_vision_explicit_override() {
        let profile_with_vision = ModelProfile {
            name: "test".to_string(),
            provider: "test".to_string(),
            model_name: "gpt-3.5-turbo".to_string(),
            base_url: "http://test".to_string(),
            api_key: "test".to_string(),
            max_tokens: 1000,
            context_length: 4000,
            adapt: None,
            vision: Some(true),
            ..Default::default()
        };
        assert!(model_has_vision(&profile_with_vision));

        let profile_without_vision = ModelProfile {
            name: "test".to_string(),
            provider: "test".to_string(),
            model_name: "gpt-4o".to_string(),
            base_url: "http://test".to_string(),
            api_key: "test".to_string(),
            max_tokens: 1000,
            context_length: 4000,
            adapt: None,
            vision: Some(false),
            ..Default::default()
        };
        assert!(!model_has_vision(&profile_without_vision));
    }

    #[test]
    fn test_model_has_vision_inferred() {
        let profile = ModelProfile {
            name: "test".to_string(),
            provider: "test".to_string(),
            model_name: "gpt-4o".to_string(),
            base_url: "http://test".to_string(),
            api_key: "test".to_string(),
            max_tokens: 1000,
            context_length: 4000,
            adapt: None,
            vision: None,
            ..Default::default()
        };
        assert!(model_has_vision(&profile));

        let profile_no_vision = ModelProfile {
            name: "test".to_string(),
            provider: "test".to_string(),
            model_name: "gpt-3.5-turbo".to_string(),
            base_url: "http://test".to_string(),
            api_key: "test".to_string(),
            max_tokens: 1000,
            context_length: 4000,
            adapt: None,
            vision: None,
            ..Default::default()
        };
        assert!(!model_has_vision(&profile_no_vision));
    }
}
