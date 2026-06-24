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
                    Some("input_json_delta") => {
                        if let Some(BlockKind::ToolUse(id)) = self.blocks.get(&index) {
                            out.push(StreamEvent::ToolUseInputDelta {
                                id: id.clone(),
                                partial_json: str_at(delta, "partial_json"),
                            });
                        }
                    }
                    _ => {} // signature_delta o. Ä. — Phase 1 ignoriert
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
        let messages =
            merge_consecutive_roles(req.messages.iter().filter_map(message_to_json).collect());
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
        if req.thinking != ThinkingLevel::Off && req.model.supports_reasoning {
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

fn message_to_json(msg: &Message) -> Option<Value> {
    let role = match msg.role {
        Role::Assistant => "assistant",
        Role::User | Role::Tool => "user",
        Role::System => return None, // System-Prompt ist ein eigenes Feld
    };
    let content: Vec<Value> = msg.content.iter().filter_map(block_to_json).collect();
    if content.is_empty() {
        return None;
    }
    Some(json!({ "role": role, "content": content }))
}

fn block_to_json(b: &ContentBlock) -> Option<Value> {
    match b {
        ContentBlock::Text { text } if text.trim().is_empty() => None,
        ContentBlock::Thinking { .. } => None, // Phase 1: nicht zurücksenden
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
    use super::merge_consecutive_roles;
    use serde_json::json;

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
