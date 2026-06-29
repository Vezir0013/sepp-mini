//! z.ai (Zhipu/GLM) — dedizierter Connector. **Eigenständiger Provider** mit eigenem `name()`,
//! eigener Key-/Endpunkt-Auflösung und eigenen Fehlertexten — kein „Dialekt-Flag" mehr auf dem
//! OpenAI-Adapter. Das Drahtformat ist OpenAI-kompatibel (Chat-Completions, SSE), deshalb teilt
//! sich dieser Connector den Decoder und den Request-Builder mit [`crate::openai`]
//! ([`build_chat_body`] / [`stream_chat`]) — dupliziert wird nichts, getrennt ist die Identität.
//! Auth aus `ZAI_API_KEY` (Format `id.secret`, Pflicht), Endpunkt aus `ZAI_BASE_URL`. Feature `zai`.

use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use sepp_core::Result;

use crate::openai::{build_chat_body, stream_chat, OpenAiDialect};
use crate::{CompletionRequest, Provider, StreamEvent};

/// z.ai (Zhipu/GLM) spricht den OpenAI-kompatiblen Chat-Completions-Endpunkt; das ist der
/// internationale Default-Host. Über `ZAI_BASE_URL` überschreibbar (z. B. die China-Region
/// `https://open.bigmodel.cn/api/paas/v4`).
const ZAI_BASE_URL: &str = "https://api.z.ai/api/paas/v4";

/// Connector für z.ai / Zhipu-GLM. Anders als bei lokalen OpenAI-Endpunkten ist der Key Pflicht.
pub struct ZaiProvider {
    client: reqwest::Client,
    api_key: Option<String>,
    base_url: String,
}

impl ZaiProvider {
    /// Neuer Connector mit optionalem API-Key und base_url. Der Key ist bei z.ai praktisch Pflicht
    /// — fehlt er, antwortet der Endpunkt mit HTTP 401; `Option` bleibt aus Symmetrie zu den
    /// anderen Providern (und für Tests).
    pub fn new(api_key: Option<String>, base_url: impl Into<String>) -> Self {
        ZaiProvider {
            client: reqwest::Client::new(),
            api_key,
            base_url: base_url.into(),
        }
    }

    /// z.ai aus der Umgebung: Key aus `ZAI_API_KEY` (Format `id.secret`), base_url aus
    /// `ZAI_BASE_URL` (Default `https://api.z.ai/api/paas/v4`).
    pub fn from_env() -> Result<Self> {
        let base = std::env::var("ZAI_BASE_URL").unwrap_or_else(|_| ZAI_BASE_URL.to_string());
        let key = std::env::var("ZAI_API_KEY").ok().filter(|k| !k.is_empty());
        Ok(Self::new(key, base))
    }
}

#[async_trait::async_trait]
impl Provider for ZaiProvider {
    fn name(&self) -> &str {
        "zai"
    }

    async fn stream<'a>(
        &'a self,
        req: CompletionRequest<'a>,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'a, StreamEvent>> {
        // Zai-Dialekt: sendet das binäre `thinking`-Objekt bei reasoning-fähigen Modellen.
        let body = build_chat_body(&req, OpenAiDialect::Zai);
        stream_chat(
            &self.client,
            &self.base_url,
            self.api_key.as_deref(),
            body,
            "zai",
            cancel,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sepp_core::{Model, ThinkingLevel};
    use serde_json::json;

    fn glm(reasoning: bool) -> Model {
        Model {
            id: "glm-5.2".into(),
            provider: "zai".into(),
            display_name: "GLM-5.2".into(),
            context_window: 200_000,
            max_output_tokens: 32_000,
            supports_reasoning: reasoning,
            supports_images: false,
        }
    }

    #[test]
    fn name_is_zai_not_openai() {
        // Der Kern der Trennung: ein z.ai-Fehler darf nicht als „openai" erscheinen.
        let p = ZaiProvider::new(None, "https://example.invalid");
        assert_eq!(p.name(), "zai");
    }

    #[test]
    fn from_env_defaults_to_zai_host_when_base_unset() {
        // Ohne ZAI_BASE_URL der internationale z.ai-Host — NICHT api.openai.com.
        std::env::remove_var("ZAI_BASE_URL");
        let p = ZaiProvider::from_env().expect("from_env");
        assert_eq!(p.base_url, ZAI_BASE_URL);
    }

    #[test]
    fn body_enables_thinking_for_reasoning_model() {
        let m = glm(true);
        let req = CompletionRequest {
            model: &m,
            system: None,
            messages: &[],
            tools: &[],
            thinking: ThinkingLevel::Medium,
            max_tokens: 8192,
        };
        let body = build_chat_body(&req, OpenAiDialect::Zai);
        assert_eq!(body["thinking"], json!({ "type": "enabled" }));
        assert_eq!(body["model"], json!("glm-5.2"));
    }
}
