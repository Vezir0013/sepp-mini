//! `sepp-provider` — normalisiert LLM-Anbieter hinter einem Trait.
//!
//! Provider übersetzen einen [`CompletionRequest`] in das anbieterspezifische Format,
//! konsumieren die (i. d. R. SSE-)Streaming-Antwort und mappen sie auf eine einheitliche
//! [`StreamEvent`]-Folge.

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use sepp_core::{Message, Model, Result, ThinkingLevel, ToolSpec, Usage};

#[cfg(feature = "anthropic")]
pub mod anthropic;
pub mod models;
#[cfg(feature = "openai")]
pub mod openai;
pub mod sse;
#[cfg(feature = "zai")]
pub mod zai;

#[cfg(feature = "anthropic")]
pub use anthropic::{decode_anthropic_sse, AnthropicProvider};
#[cfg(feature = "openai")]
pub use openai::{decode_openai_sse, OpenAiDialect, OpenAiProvider};
#[cfg(feature = "zai")]
pub use zai::ZaiProvider;

/// Ein normalisiertes Streaming-Ereignis. Die Reihenfolge-Invariante:
/// `MessageStart (TextDelta|ThinkingDelta|ThinkingSignature|ToolUse*)* Usage? MessageStop`.
/// `ToolUseStop` kann auch mitten im Stream kommen (Server, die den tool_call-`index`
/// recyceln, schließen den vorigen Call beim Start des nächsten), bleibt aber immer
/// innerhalb der `ToolUse*`-Gruppe vor `Usage`/`MessageStop`. `ThinkingSignature` schließt
/// die vorangehenden `ThinkingDelta`s zu einem signierten Block ab (Anthropic signiert je
/// Thinking-Block; die Signatur MUSS beim Zurücksenden unverändert mitkommen).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart,
    TextDelta { text: String },
    ThinkingDelta { text: String },
    ThinkingSignature { signature: String },
    ToolUseStart { id: String, name: String },
    ToolUseInputDelta { id: String, partial_json: String },
    ToolUseStop { id: String },
    Usage(Usage),
    MessageStop { stop_reason: StopReason },
    Error { message: String },
}

/// Grund für das Ende eines Turns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    #[default]
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

impl StopReason {
    /// Mappt Anthropics `stop_reason`-Strings; Unbekanntes → `EndTurn`.
    pub fn from_anthropic(s: &str) -> Self {
        match s {
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            _ => StopReason::EndTurn,
        }
    }
}

/// Ein Completion-Request. Borrowt Modell/Messages/Tools, um Kopien zu vermeiden.
#[derive(Debug, Clone)]
pub struct CompletionRequest<'a> {
    pub model: &'a Model,
    pub system: Option<&'a str>,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub thinking: ThinkingLevel,
    pub max_tokens: u64,
}

/// Abstraktion über einen LLM-Anbieter.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Anbieter-Name, z. B. `"anthropic"`.
    fn name(&self) -> &str;

    /// Streamt eine Completion. Der Stream endet mit `MessageStop` oder `Error`.
    async fn stream<'a>(
        &'a self,
        req: CompletionRequest<'a>,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'a, StreamEvent>>;
}
