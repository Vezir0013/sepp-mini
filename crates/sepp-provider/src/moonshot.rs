//! Moonshot AI (Kimi) — dedizierter Connector. **Eigenständiger Provider** mit eigenem `name()`,
//! eigener Key-/Endpunkt-Auflösung und eigenen Fehlertexten. Das Drahtformat ist
//! OpenAI-kompatibel (Chat-Completions, SSE), deshalb teilt sich dieser Connector den Decoder und
//! den Request-Builder mit [`crate::openai`] ([`build_chat_body`] / [`stream_chat`]) — dupliziert
//! wird nichts, getrennt ist die Identität. Die zwei Abweichungen im Body
//! (`max_completion_tokens` statt `max_tokens`, `reasoning_effort` als Kostenregler) stecken in
//! [`OpenAiDialect::Moonshot`].
//!
//! Auth aus `MOONSHOT_API_KEY` (Pflicht), Endpunkt aus `MOONSHOT_BASE_URL`. Feature `moonshot`.
//!
//! **Kein 4xx-Retry.** [`crate::openai::OpenAiProvider`] wiederholt einen mit 4xx abgelehnten
//! Request einmal ohne `reasoning_effort`; dieser Connector ruft `stream_chat` direkt auf und
//! erbt das bewusst nicht. Der Fallback existiert für die heterogene Population lokaler Server
//! (Ollama/vLLM), bei denen Feldunterstützung unbekannt ist — Moonshot ist eine einzige,
//! dokumentierte Cloud-API. Auf einer Bezahl-API wäre ein blinder 4xx-Retry sogar schädlich:
//! `is_client_error()` trifft auch 401 (zweiter sinnloser Call mit falschem Key) und 429
//! (verdoppelt genau das, was gerade limitiert wird), und der echte Fehler würde durch einen
//! zweiten, anders aussehenden ersetzt.

use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use sepp_core::Result;

use crate::openai::{
    build_chat_body, nonempty_trimmed, resolve_base_url, stream_chat, OpenAiDialect,
};
use crate::{CompletionRequest, Provider, StreamEvent};

/// Moonshots internationaler OpenAI-kompatibler Endpunkt. Über `MOONSHOT_BASE_URL`
/// überschreibbar (z. B. die China-Region `https://api.moonshot.cn/v1` oder ein Gateway).
const MOONSHOT_BASE_URL: &str = "https://api.moonshot.ai/v1";

/// Connector für Moonshot AI / Kimi. Anders als bei lokalen OpenAI-Endpunkten ist der Key Pflicht.
pub struct MoonshotProvider {
    client: reqwest::Client,
    api_key: Option<String>,
    base_url: String,
}

impl MoonshotProvider {
    /// Neuer Connector mit optionalem API-Key und base_url. Der Key ist bei Moonshot praktisch
    /// Pflicht — fehlt er, antwortet der Endpunkt mit HTTP 401; `Option` bleibt aus Symmetrie zu
    /// den anderen Providern (und für Tests).
    pub fn new(api_key: Option<String>, base_url: impl Into<String>) -> Self {
        MoonshotProvider {
            client: reqwest::Client::new(),
            api_key,
            base_url: base_url.into(),
        }
    }

    /// Moonshot aus der Umgebung: Key aus `MOONSHOT_API_KEY`, base_url aus `MOONSHOT_BASE_URL`
    /// (Default `https://api.moonshot.ai/v1`).
    pub fn from_env() -> Result<Self> {
        let base = resolve_base_url(std::env::var("MOONSHOT_BASE_URL").ok(), MOONSHOT_BASE_URL);
        let key = nonempty_trimmed(std::env::var("MOONSHOT_API_KEY").ok());
        Ok(Self::new(key, base))
    }
}

#[async_trait::async_trait]
impl Provider for MoonshotProvider {
    fn name(&self) -> &str {
        "moonshot"
    }

    async fn stream<'a>(
        &'a self,
        req: CompletionRequest<'a>,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'a, StreamEvent>> {
        // Moonshot-Dialekt: `max_completion_tokens` statt `max_tokens`, plus `reasoning_effort`
        // bei reasoning-fähigen Modellen.
        let body = build_chat_body(&req, OpenAiDialect::Moonshot);
        stream_chat(
            &self.client,
            &self.base_url,
            self.api_key.as_deref(),
            body,
            "moonshot",
            cancel,
        )
        .await
        .map_err(|e| e.into_sepp())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sepp_core::{Model, ThinkingLevel};
    use serde_json::json;

    fn kimi(reasoning: bool) -> Model {
        Model {
            id: "kimi-k3".into(),
            provider: "moonshot".into(),
            display_name: "Kimi K3".into(),
            context_window: 1_048_576,
            max_output_tokens: 32_768,
            supports_reasoning: reasoning,
            supports_images: false,
        }
    }

    fn req(model: &Model, thinking: ThinkingLevel) -> CompletionRequest<'_> {
        CompletionRequest {
            model,
            system: None,
            messages: &[],
            tools: &[],
            thinking,
            max_tokens: 32_768,
        }
    }

    #[test]
    fn name_is_moonshot_not_openai() {
        // Der Kern der Trennung: ein Moonshot-Fehler darf nicht als „openai" erscheinen.
        let p = MoonshotProvider::new(None, "https://example.invalid");
        assert_eq!(p.name(), "moonshot");
    }

    #[test]
    fn base_url_resolution_defaults_to_moonshot_host() {
        // Ohne/mit leerem MOONSHOT_BASE_URL der Moonshot-Host — NICHT api.openai.com.
        // Pure Auflösung statt env-mutierendem remove_var (racet im parallelen Test-Binary).
        assert_eq!(resolve_base_url(None, MOONSHOT_BASE_URL), MOONSHOT_BASE_URL);
        assert_eq!(
            resolve_base_url(Some(String::new()), MOONSHOT_BASE_URL),
            MOONSHOT_BASE_URL
        );
    }

    #[test]
    fn body_uses_max_completion_tokens_and_reasoning_effort() {
        let m = kimi(true);
        let body = build_chat_body(&req(&m, ThinkingLevel::Medium), OpenAiDialect::Moonshot);
        assert_eq!(body["model"], json!("kimi-k3"));
        assert_eq!(body["max_completion_tokens"], json!(32_768));
        assert!(body.get("max_tokens").is_none(), "{body}");
        assert_eq!(body["reasoning_effort"], json!("high"));
    }

    #[test]
    fn thinking_off_sends_low_effort_not_absence() {
        // Moonshot kann Thinking nicht abschalten. Ein weggelassenes Feld hieße Server-Default
        // `max` — also das Gegenteil dessen, was `--no-think` ausdrückt.
        let m = kimi(true);
        let body = build_chat_body(&req(&m, ThinkingLevel::Off), OpenAiDialect::Moonshot);
        assert_eq!(body["reasoning_effort"], json!("low"));
    }
}
