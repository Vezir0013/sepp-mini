//! Anthropic-Messages-API-Adapter (Phase 1): baut den Request, streamt SSE und mappt
//! die anbieterspezifischen Events auf [`StreamEvent`]. Auth aus `ANTHROPIC_API_KEY`.

use std::collections::{HashMap, VecDeque};

use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use sepp_core::{ContentBlock, Message, Result, Role, SeppError, ThinkingLevel, Usage};

use crate::sse::SseDecoder;
use crate::{CompletionRequest, Provider, StopReason, StreamEvent};

const API_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Welche Art Content-Block sich hinter einem `index` verbirgt (für Delta-Zuordnung).
#[derive(Debug)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse(String),
}

/// Übersetzt die Anthropic-SSE-JSON-Events (zustandsbehaftet) in [`StreamEvent`].
#[derive(Debug, Default)]
pub struct AnthropicMapper {
    blocks: HashMap<u64, BlockKind>,
    usage: Usage,
    stop_reason: StopReason,
}

impl AnthropicMapper {
    /// Verarbeitet ein geparstes SSE-`data`-JSON und liefert die resultierenden Events.
    pub fn push(&mut self, v: &Value) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                    self.usage.input_tokens = u64_at(u, "input_tokens");
                    self.usage.cache_read_tokens = u64_at(u, "cache_read_input_tokens");
                    self.usage.cache_write_tokens = u64_at(u, "cache_creation_input_tokens");
                }
                out.push(StreamEvent::MessageStart);
            }
            "content_block_start" => {
                let index = u64_at(v, "index");
                let cb = v.get("content_block");
                match cb.and_then(|c| c.get("type")).and_then(Value::as_str) {
                    Some("tool_use") => {
                        let id = str_at(cb, "id");
                        let name = str_at(cb, "name");
                        self.blocks.insert(index, BlockKind::ToolUse(id.clone()));
                        out.push(StreamEvent::ToolUseStart { id, name });
                    }
                    Some("thinking") => {
                        self.blocks.insert(index, BlockKind::Thinking);
                    }
                    _ => {
                        self.blocks.insert(index, BlockKind::Text);
                    }
                }
            }
            "content_block_delta" => {
                let index = u64_at(v, "index");
                let delta = v.get("delta");
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => out.push(StreamEvent::TextDelta {
                        text: str_at(delta, "text"),
                    }),
                    Some("thinking_delta") => out.push(StreamEvent::ThinkingDelta {
                        text: str_at(delta, "thinking"),
                    }),
                    // Kommt als EIN Delta am Ende des Thinking-Blocks und schließt ihn ab.
                    // Die Signatur muss beim Zurücksenden unverändert mitkommen (Tool-Use-
                    // Fortsetzung), sonst lehnt die API den Folge-Request mit 400 ab.
                    Some("signature_delta") => out.push(StreamEvent::ThinkingSignature {
                        signature: str_at(delta, "signature"),
                    }),
                    Some("input_json_delta") => {
                        if let Some(BlockKind::ToolUse(id)) = self.blocks.get(&index) {
                            out.push(StreamEvent::ToolUseInputDelta {
                                id: id.clone(),
                                partial_json: str_at(delta, "partial_json"),
                            });
                        }
                    }
                    _ => {} // Unbekannte Delta-Typen ignorieren (vorwärtskompatibel)
                }
            }
            "content_block_stop" => {
                let index = u64_at(v, "index");
                if let Some(BlockKind::ToolUse(id)) = self.blocks.get(&index) {
                    out.push(StreamEvent::ToolUseStop { id: id.clone() });
                }
            }
            "message_delta" => {
                if let Some(sr) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = StopReason::from_anthropic(sr);
                }
                if let Some(ot) = v
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                {
                    self.usage.output_tokens = ot;
                }
                out.push(StreamEvent::Usage(self.usage));
            }
            "message_stop" => out.push(StreamEvent::MessageStop {
                stop_reason: self.stop_reason,
            }),
            "error" => out.push(StreamEvent::Error {
                message: v
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string(),
            }),
            _ => {} // ping etc.
        }
        out
    }
}

fn u64_at(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn str_at(v: Option<&Value>, key: &str) -> String {
    v.and_then(|x| x.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Dekodiert einen kompletten rohen Anthropic-SSE-Body zu [`StreamEvent`]s.
/// Nützlich für Fixture-/Replay-Tests ohne Netz.
pub fn decode_anthropic_sse(raw: &[u8]) -> Vec<StreamEvent> {
    let mut dec = SseDecoder::new();
    let mut map = AnthropicMapper::default();
    let mut out = Vec::new();
    let mut feed = |payloads: Vec<String>, out: &mut Vec<StreamEvent>| {
        for payload in payloads {
            if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                out.extend(map.push(&v));
            }
        }
    };
    let pushed = dec.push(raw);
    feed(pushed, &mut out);
    let finished = dec.finish();
    feed(finished, &mut out);
    out
}

/// Adapter für die Anthropic-Messages-API.
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl AnthropicProvider {
    /// Erzeugt einen Provider mit explizitem API-Key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Liest den API-Key aus `ANTHROPIC_API_KEY`.
    pub fn from_env() -> Result<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| SeppError::Config("ANTHROPIC_API_KEY nicht gesetzt".into()))?;
        if key.trim().is_empty() {
            return Err(SeppError::Config("ANTHROPIC_API_KEY ist leer".into()));
        }
        Ok(Self::new(key))
    }

    /// Überschreibt die Basis-URL (z. B. für Tests/Proxies).
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    fn build_body(&self, req: &CompletionRequest) -> Value {
        let max_tokens = if req.max_tokens > 0 {
            req.max_tokens
        } else {
            req.model.max_output_tokens
        };
        // Ein Flag für Request-Parameter UND History-Serialisierung: Thinking-Blöcke dürfen
        // nur in Requests erscheinen, die den thinking-Parameter setzen — sonst 400 („must
        // have thinking enabled"). Betroffen sind sonst compact() (summarisiert mit Off),
        // /think off, --resume ohne --think und Modelle ohne supports_reasoning.
        let thinking_enabled = req.thinking != ThinkingLevel::Off && req.model.supports_reasoning;
        let messages = merge_consecutive_roles(
            req.messages
                .iter()
                .filter_map(|m| message_to_json(m, thinking_enabled))
                .collect(),
        );
        let mut body = json!({
            "model": req.model.id,
            "max_tokens": max_tokens,
            "stream": true,
            "messages": messages,
        });
        if let Some(sys) = req.system {
            if !sys.is_empty() {
                body["system"] = json!(sys);
            }
        }
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }
        if thinking_enabled {
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": req.thinking.budget_tokens(),
            });
        }
        body
    }
}

/// Führt aufeinanderfolgende Messages gleicher Rolle zusammen (Anthropic verlangt
/// abwechselnde Rollen). Tritt z. B. nach Compaction auf: `user(summary)` + `user(prompt)`.
fn merge_consecutive_roles(msgs: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(msgs.len());
    for m in msgs {
        if let Some(last) = out.last_mut() {
            if last.get("role") == m.get("role") {
                if let (Some(a), Some(b)) =
                    (last["content"].as_array_mut(), m["content"].as_array())
                {
                    a.extend(b.iter().cloned());
                    continue;
                }
            }
        }
        out.push(m);
    }
    out
}

fn message_to_json(msg: &Message, thinking_enabled: bool) -> Option<Value> {
    let role = match msg.role {
        Role::Assistant => "assistant",
        Role::User | Role::Tool => "user",
        Role::System => return None, // System-Prompt ist ein eigenes Feld
    };
    let content: Vec<Value> = msg
        .content
        .iter()
        .filter_map(|b| block_to_json(b, thinking_enabled))
        .collect();
    if content.is_empty() {
        return None;
    }
    Some(json!({ "role": role, "content": content }))
}

fn block_to_json(b: &ContentBlock, thinking_enabled: bool) -> Option<Value> {
    match b {
        ContentBlock::Text { text } if text.trim().is_empty() => None,
        // Signierte Thinking-Blöcke MÜSSEN unverändert zurück, solange der Request Thinking
        // aktiviert: Bei Tool-Use lehnt die API den Folge-Request mit 400 ab, wenn der letzte
        // Assistant-Turn seinen Thinking-Block verliert. Frühere Turns entfernt die API
        // selbst (nicht als Input berechnet) — mitsenden ist dann korrekt UND kostenfrei.
        // Wire-Feld heißt `thinking`, nicht `text` — daher explizit statt serde-Derive.
        ContentBlock::Thinking {
            text,
            signature: Some(sig),
        } if thinking_enabled => Some(json!({
            "type": "thinking",
            "thinking": text,
            "signature": sig,
        })),
        // Alle übrigen Thinking-Blöcke werden weggelassen: unsignierte (Fremd-Provider-
        // Reasoning, Alt-Sessions aus Phase 1) lehnt die API immer ab, und bei deaktiviertem
        // Thinking sind auch signierte verboten („Requests which include thinking blocks
        // must have thinking enabled", 400) — sonst bräche jede Compaction (summarisiert
        // mit Off), /think off und --resume ohne --think nach einem Thinking-Turn.
        ContentBlock::Thinking { .. } => None,
        other => serde_json::to_value(other).ok(),
    }
}

/// Zustand des Live-Decode-Streams (`unfold`).
struct DecodeState {
    bytes: BoxStream<'static, reqwest::Result<Bytes>>,
    decoder: SseDecoder,
    mapper: AnthropicMapper,
    pending: VecDeque<StreamEvent>,
    finished: bool,
    cancel: CancellationToken,
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn stream<'a>(
        &'a self,
        req: CompletionRequest<'a>,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'a, StreamEvent>> {
        let body = self.build_body(&req);
        let url = format!("{}/v1/messages", self.base_url);

        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| SeppError::Provider(format!("anthropic request: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(SeppError::Provider(format!(
                "anthropic: HTTP {status}: {}",
                text.trim()
            )));
        }

        let state = DecodeState {
            bytes: Box::pin(resp.bytes_stream()),
            decoder: SseDecoder::new(),
            mapper: AnthropicMapper::default(),
            pending: VecDeque::new(),
            finished: false,
            cancel,
        };

        let stream = futures::stream::unfold(state, |mut st| async move {
            loop {
                if let Some(ev) = st.pending.pop_front() {
                    return Some((ev, st));
                }
                if st.finished {
                    return None;
                }
                tokio::select! {
                    _ = st.cancel.cancelled() => return None,
                    chunk = st.bytes.next() => match chunk {
                        Some(Ok(b)) => {
                            for payload in st.decoder.push(&b) {
                                if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                                    for ev in st.mapper.push(&v) {
                                        st.pending.push_back(ev);
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            st.pending.push_back(StreamEvent::Error {
                                message: format!("anthropic stream: {e}"),
                            });
                            // Stream beenden: durch leeren Stream ersetzen → der nächste
                            // Poll läuft in den None-Zweig und setzt `finished`.
                            st.bytes = Box::pin(futures::stream::empty());
                        }
                        None => {
                            for payload in st.decoder.finish() {
                                if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                                    for ev in st.mapper.push(&v) {
                                        st.pending.push_back(ev);
                                    }
                                }
                            }
                            st.finished = true;
                        }
                    }
                }
            }
        });

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::{block_to_json, merge_consecutive_roles, AnthropicProvider};
    use crate::CompletionRequest;
    use sepp_core::{ContentBlock, Message, Model, Role, ThinkingLevel};
    use serde_json::json;

    #[test]
    fn signed_thinking_block_uses_wire_format() {
        // Rückgabe im Anthropic-Drahtformat: Feld heißt `thinking` (nicht `text` wie im
        // sepp-core-Block), Signatur unverändert — sonst 400 bei Tool-Use-Fortsetzung.
        let b = ContentBlock::Thinking {
            text: "Denkprozess".into(),
            signature: Some("sig123".into()),
        };
        assert_eq!(
            block_to_json(&b, true),
            Some(json!({
                "type": "thinking",
                "thinking": "Denkprozess",
                "signature": "sig123",
            }))
        );
    }

    #[test]
    fn unsigned_thinking_block_is_dropped() {
        // Ohne Signatur (Fremd-Provider-Reasoning, Alt-Sessions) lehnt die API den Block
        // ab — er darf nicht in den Request, egal ob Thinking an ist.
        let b = ContentBlock::Thinking {
            text: "lokales Reasoning".into(),
            signature: None,
        };
        assert_eq!(block_to_json(&b, true), None);
        assert_eq!(block_to_json(&b, false), None);
    }

    #[test]
    fn signed_thinking_block_dropped_when_thinking_disabled() {
        // Bei deaktiviertem Thinking lehnt die API Requests MIT thinking-Blöcken ab
        // („must have thinking enabled") — auch signierte müssen dann raus.
        let b = ContentBlock::Thinking {
            text: "Denkprozess".into(),
            signature: Some("sig123".into()),
        };
        assert_eq!(block_to_json(&b, false), None);
    }

    fn reasoning_model() -> Model {
        Model {
            id: "claude-test".into(),
            provider: "anthropic".into(),
            display_name: "Test".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            supports_reasoning: true,
            supports_images: false,
        }
    }

    /// History mit einem signierten Thinking-Turn — der Zustand nach jedem Thinking+Tool-Use.
    fn thinking_history() -> Vec<Message> {
        vec![
            Message::user_text("hallo"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        text: "Denkprozess".into(),
                        signature: Some("sig123".into()),
                    },
                    ContentBlock::Text {
                        text: "Antwort".into(),
                    },
                ],
                usage: None,
            },
        ]
    }

    #[test]
    fn body_keeps_signed_thinking_when_enabled() {
        let p = AnthropicProvider::new("k");
        let model = reasoning_model();
        let msgs = thinking_history();
        let body = p.build_body(&CompletionRequest {
            model: &model,
            system: None,
            messages: &msgs,
            tools: &[],
            thinking: ThinkingLevel::Medium,
            max_tokens: 8192,
        });
        assert!(body.get("thinking").is_some());
        let assistant = &body["messages"][1]["content"];
        assert_eq!(assistant[0]["type"], "thinking");
        assert_eq!(assistant[0]["signature"], "sig123");
    }

    #[test]
    fn body_drops_signed_thinking_when_disabled() {
        // Der compact()-Pfad: summarisiert IMMER mit ThinkingLevel::Off über die volle
        // History — ohne den Drop würde jede Compaction nach einem Thinking-Turn 400en.
        let p = AnthropicProvider::new("k");
        let model = reasoning_model();
        let msgs = thinking_history();
        let body = p.build_body(&CompletionRequest {
            model: &model,
            system: None,
            messages: &msgs,
            tools: &[],
            thinking: ThinkingLevel::Off,
            max_tokens: 8192,
        });
        assert!(body.get("thinking").is_none());
        let rendered = body["messages"].to_string();
        assert!(!rendered.contains("thinking"), "{rendered}");
        assert!(rendered.contains("Antwort")); // Text-Block bleibt
    }

    #[test]
    fn merges_consecutive_same_role() {
        let input = vec![
            json!({"role":"user","content":[{"type":"text","text":"a"}]}),
            json!({"role":"user","content":[{"type":"text","text":"b"}]}),
            json!({"role":"assistant","content":[{"type":"text","text":"c"}]}),
        ];
        let out = merge_consecutive_roles(input);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(out[1]["role"], "assistant");
    }
}
