//! Statische Tabelle bekannter Modelle.
//!
//! HINWEIS: Die Model-IDs/Limits sind die zum Erstellungszeitpunkt aktuellen Werte (Anthropic
//! bzw. z.ai/Zhipu-GLM). Vor produktivem Live-Einsatz gegen die jeweilige Anbieter-API
//! verifizieren; Custom-Modelle kommen (Phase 5) aus `~/.sepp/models.toml`.

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

/// z.ai / Zhipu-GLM-Modell. Läuft über den OpenAI-kompatiblen Adapter (`--provider zai`),
/// daher reine Textmodelle hier (Vision-Variante GLM-4.5V ist separat und noch ungetestet).
fn zai(id: &str, display_name: &str, context_window: u64, max_output_tokens: u64) -> Model {
    Model {
        id: id.to_string(),
        provider: "zai".to_string(),
        display_name: display_name.to_string(),
        context_window,
        max_output_tokens,
        supports_reasoning: true,
        supports_images: false,
    }
}

/// Eingebaute Modelle.
pub fn builtin_models() -> Vec<Model> {
    vec![
        anthropic("claude-opus-4-8", "Claude Opus 4.8", 200_000, 32_000),
        anthropic("claude-sonnet-4-6", "Claude Sonnet 4.6", 200_000, 64_000),
        anthropic("claude-haiku-4-5", "Claude Haiku 4.5", 200_000, 32_000),
        // z.ai / Zhipu GLM (OpenAI-kompatibler Endpunkt). glm-5.2 ist das aktuelle Flaggschiff
        // und der Default für --provider zai. Kontextfenster/max-output bewusst konservativ
        // gehalten (früher komprimieren statt überlaufen) und gegen die z.ai-Docs zu
        // verifizieren — siehe HINWEIS oben.
        zai("glm-5.2", "GLM-5.2", 200_000, 32_000),
        zai("glm-4.6", "GLM-4.6", 200_000, 32_000),
        zai("glm-4.5-air", "GLM-4.5-Air", 128_000, 32_000),
        zai("glm-4.5-flash", "GLM-4.5-Flash", 128_000, 32_000),
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

    #[test]
    fn registry_includes_zai_glm_models() {
        // Flaggschiff/Default für --provider zai.
        let flagship = find_model("glm-5.2").expect("glm-5.2 ist registriert");
        assert_eq!(flagship.provider, "zai");
        assert_eq!(flagship.context_window, 200_000);

        let glm = find_model("glm-4.6").expect("glm-4.6 ist registriert");
        assert_eq!(glm.provider, "zai");
        assert!(!glm.supports_images);
        assert!(builtin_models().iter().any(|m| m.id == "glm-4.5-flash"));
    }
}
