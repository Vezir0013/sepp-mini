//! Statische Tabelle bekannter Modelle.
//!
//! HINWEIS: Die Model-IDs sind die zum Erstellungszeitpunkt aktuellen Anthropic-IDs.
//! Vor produktivem Live-Einsatz gegen die echte Anthropic-API verifizieren; Custom-Modelle
//! kommen (Phase 5) aus `~/.sepp/models.toml`.

use sepp_core::Model;

/// Default-Modell, wenn keines per CLI/Env gewählt wird.
pub const DEFAULT_MODEL_ID: &str = "claude-sonnet-4-6";

fn anthropic(id: &str, display_name: &str, context_window: u64, max_output_tokens: u64) -> Model {
    Model {
        id: id.to_string(),
        provider: "anthropic".to_string(),
        display_name: display_name.to_string(),
        context_window,
        max_output_tokens,
        supports_reasoning: true,
        supports_images: true,
    }
}

/// Eingebaute Modelle.
pub fn builtin_models() -> Vec<Model> {
    vec![
        anthropic("claude-opus-4-8", "Claude Opus 4.8", 200_000, 32_000),
        anthropic("claude-sonnet-4-6", "Claude Sonnet 4.6", 200_000, 64_000),
        anthropic("claude-haiku-4-5", "Claude Haiku 4.5", 200_000, 32_000),
    ]
}

/// Findet ein eingebautes Modell anhand seiner ID.
pub fn find_model(id: &str) -> Option<Model> {
    builtin_models().into_iter().find(|m| m.id == id)
}

/// Liefert das Default-Modell (panikfrei: Fallback konstruiert es direkt).
pub fn default_model() -> Model {
    find_model(DEFAULT_MODEL_ID)
        .unwrap_or_else(|| anthropic("claude-sonnet-4-6", "Claude Sonnet 4.6", 200_000, 64_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_model_exists() {
        assert_eq!(default_model().id, DEFAULT_MODEL_ID);
    }
}
