//! Statische Tabelle bekannter Modelle.
//!
//! HINWEIS: Die Model-IDs/Limits sind die zum Erstellungszeitpunkt aktuellen Werte (Anthropic,
//! z.ai/Zhipu-GLM, Moonshot/Kimi). Vor produktivem Live-Einsatz gegen die jeweilige Anbieter-API
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

/// z.ai / Zhipu-GLM-Modell. Bedient vom dedizierten z.ai-Connector (`--provider zai`, der das
/// OpenAI-kompatible Drahtformat teilt); daher reine Textmodelle hier (Vision-Variante GLM-4.5V
/// ist separat und noch ungetestet).
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

/// Moonshot-AI-Modell der Kimi-K-Generation. Bedient vom dedizierten Moonshot-Connector
/// (`--provider moonshot`, der das OpenAI-kompatible Drahtformat teilt).
///
/// `supports_reasoning: true` heißt hier **nicht** „Reasoning ist abschaltbar" — Kimi K3 denkt
/// immer; das Flag steuert nur, ob `reasoning_effort` (low|high|max) mitgesendet wird.
///
/// `supports_images: false` trotz nativ multimodalem K3: der OpenAI-Adapter serialisiert keine
/// Bild-Blöcke (`message_to_openai`/`text_of` in `openai.rs` verwerfen sie ersatzlos). Das Flag
/// beschreibt hier den Adapter, nicht das Modell — stünde `true`, verschwände ein Bild still aus
/// dem Request und das Modell antwortete auf einen Prompt ohne Bild.
fn moonshot(id: &str, display_name: &str, context_window: u64, max_output_tokens: u64) -> Model {
    Model {
        id: id.to_string(),
        provider: "moonshot".to_string(),
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
        // Moonshot AI / Kimi (OpenAI-kompatibler Endpunkt api.moonshot.ai/v1). kimi-k3 ist das
        // Flaggschiff und der Default für --provider moonshot. Kontextfenster laut Anbieter-Doku
        // 1_048_576; max_output ist bewusst NICHT Moonshots Default (131_072), sondern eine
        // konservative Arbeitsgröße — das Feld greift nur als Fallback, wenn kein --max-tokens
        // gesetzt ist, und Moonshots Rate-Limit rechnet gegen genau diesen Wert.
        moonshot("kimi-k3", "Kimi K3", 1_048_576, 32_768),
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

    #[test]
    fn registry_includes_moonshot_kimi_k3() {
        // Der Eintrag ist das, was `sepp -m kimi-k3` OHNE --provider funktionieren lässt: die
        // CLI leitet den Provider nur für registrierte IDs aus dem Modell ab.
        let k3 = find_model("kimi-k3").expect("kimi-k3 ist registriert");
        assert_eq!(k3.provider, "moonshot");
        assert_eq!(k3.context_window, 1_048_576);
        assert!(k3.supports_reasoning);
        // Der OpenAI-Adapter serialisiert keine Bild-Blöcke — das Flag beschreibt den Adapter.
        assert!(!k3.supports_images);
    }
}
