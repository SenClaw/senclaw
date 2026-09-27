//! Wire-format adapters for providers that don't speak chat/completions.
//!
//! These live here rather than in `zen_core` because they belong to the
//! *provider* domain, not the agent runtime: each one exists because a
//! specific vendor endpoint has its own request and response shape. `zen_core`
//! keeps the two protocols it has always spoken (OpenAI chat/completions and
//! Anthropic Messages) plus the local runtimes; anything vendor-specific is
//! ported here alongside the provider registry that names it.
//!
//! | Adapter | `adapt` value | Used by |
//! |---|---|---|
//! | [`codex`] | `codex` | OpenAI Codex, Grok CLI |
//! | [`antigravity`] | `antigravity` | Google Antigravity |
//!
//! Providers whose endpoints are already OpenAI- or Anthropic-shaped
//! (GitHub Copilot, Qwen, Kimi, iFlow, every free-tier preset) need no adapter
//! here — they set `adapt` to `openai` or `anthropic` and reuse `zen_core`.
//!
//! `query_llm` dispatches on `adapt`; a value with no arm falls through to the
//! OpenAI adapter, so `every_signin_provider_routes_to_a_real_adapter` in
//! `zen_core::query_llm` asserts that every registered provider's `adapt` is
//! actually routed.

pub mod antigravity;
pub mod codex;

/// The compatible surface a provider is normalised onto.
///
/// Everything SenClaw talks to ends up as one of these two. A provider whose
/// endpoint already speaks the protocol is used directly; one that doesn't
/// gets an adapter in this module that translates in both directions. There is
/// no third protocol in the agent runtime, which is what keeps `zen_core` from
/// growing a vendor branch every time a provider is added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatFamily {
    /// OpenAI chat/completions shape.
    OpenAi,
    /// Anthropic Messages shape — also SenClaw's internal message model.
    Anthropic,
}

impl CompatFamily {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAi => "openai-compatible",
            Self::Anthropic => "anthropic-compatible",
        }
    }
}

/// Which compatible surface an `adapt` value resolves to, and whether it needs
/// a translation adapter to get there.
///
/// Returns `None` for the local in-process runtimes, which bypass HTTP
/// entirely and belong to neither family.
pub fn compat_family(adapt: &str) -> Option<(CompatFamily, bool)> {
    match adapt {
        // Native — the endpoint already speaks it.
        "openai" => Some((CompatFamily::OpenAi, false)),
        "anthropic" => Some((CompatFamily::Anthropic, false)),
        // Translated — OpenAI Responses items in, OpenAI-shaped tool calls out.
        "codex" => Some((CompatFamily::OpenAi, true)),
        // Translated — Gemini contents in, Anthropic-shaped blocks out.
        "antigravity" => Some((CompatFamily::Anthropic, true)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_signin_provider_lands_on_a_compatible_surface() {
        for p in crate::providers::oauth::provider::all() {
            assert!(
                compat_family(p.adapt).is_some(),
                "provider `{}` (adapt `{}`) normalises to neither family",
                p.id,
                p.adapt
            );
        }
    }

    #[test]
    fn every_free_tier_preset_lands_on_a_compatible_surface() {
        for p in crate::providers::all() {
            assert!(
                compat_family(p.adapt).is_some(),
                "preset `{}` (adapt `{}`) normalises to neither family",
                p.id,
                p.adapt
            );
        }
    }

    #[test]
    fn only_the_two_custom_adapters_need_translation() {
        let translated: Vec<&str> = ["openai", "anthropic", "codex", "antigravity"]
            .into_iter()
            .filter(|a| compat_family(a).is_some_and(|(_, needs)| needs))
            .collect();
        assert_eq!(translated, vec!["codex", "antigravity"]);
    }

    #[test]
    fn native_adapts_are_not_marked_as_translated() {
        assert_eq!(compat_family("openai"), Some((CompatFamily::OpenAi, false)));
        assert_eq!(
            compat_family("anthropic"),
            Some((CompatFamily::Anthropic, false))
        );
    }

    #[test]
    fn local_runtimes_belong_to_neither_family() {
        assert!(compat_family("local-mlx").is_none());
        assert!(compat_family("local-candle-native").is_none());
    }

    #[test]
    fn family_labels_match_the_registry_naming() {
        assert_eq!(CompatFamily::OpenAi.as_str(), "openai-compatible");
        assert_eq!(CompatFamily::Anthropic.as_str(), "anthropic-compatible");
    }
}
