//! OpenAI-kompatibler Adapter (Chat-Completions, SSE). Über die `base_url` deckt er auch
//! lokale Endpunkte ab (Ollama/vLLM, OpenAI-kompatibel). Auth aus `OPENAI_API_KEY` (optional
//! für lokale Server). Feature `openai`.

use std::collections::HashMap;

use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use sepp_core::{ContentBlock, Message, Result, Role, SeppError, ThinkingLevel, Usage};

use crate::sse::SseDecoder;
use crate::{CompletionRequest, Provider, StopReason, StreamEvent};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Welcher OpenAI-kompatible Dialekt gesprochen wird. Steuert anbieterspezifische Body-Felder:
/// z. B. das z.ai-`thinking`-Objekt, das echtes OpenAI als unbekanntes Feld mit HTTP 400 ablehnen
/// würde. Default ist [`OpenAiDialect::OpenAi`] (auch für lokale Endpunkte); der dedizierte
/// z.ai-Connector ([`crate::zai::ZaiProvider`]) setzt [`OpenAiDialect::Zai`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpenAiDialect {
    /// Striktes OpenAI/Chat-Completions (OpenAI, Ollama, vLLM, …) — keine Zusatzfelder.
    #[default]
    OpenAi,
    /// z.ai / Zhipu-GLM — akzeptiert zusätzlich `thinking:{type:…}`.
    Zai,
}

/// Übersetzt OpenAI-SSE-Deltas (zustandsbehaftet) in [`StreamEvent`].
#[derive(Debug, Default)]
pub struct OpenAiMapper {
    started: bool,
    stopped: bool,
    tools: HashMap<u64, String>, // index -> tool id
    order: Vec<u64>,
    stop: StopReason,
    usage: Usage,
    has_usage: bool,
}

impl OpenAiMapper {
    /// Verarbeitet einen SSE-JSON-Chunk und liefert die daraus folgenden Events.
    pub fn push(&mut self, v: &Value) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if let Some(choices) = v.get("choices").and_then(Value::as_array) {
            for ch in choices {
                if !self.started {
                    self.started = true;
                    out.push(StreamEvent::MessageStart);
                }
                let delta = ch.get("delta");
                if let Some(c) = delta.and_then(|d| d.get("content")).and_then(Value::as_str) {
                    if !c.is_empty() {
                        out.push(StreamEvent::TextDelta {
                            text: c.to_string(),
                        });
                    }
                }
                // Reasoning-Modelle über OpenAI-kompatible Endpunkte (z. B. z.ai GLM-4.6,
                // DeepSeek-R1) streamen ihr Denken in `reasoning_content` statt `content`.
                // Als ThinkingDelta abbilden statt verwerfen; für reine Chat-Modelle (kein
                // solches Feld) ist der Zweig ein No-op.
                if let Some(rc) = delta
                    .and_then(|d| d.get("reasoning_content"))
                    .and_then(Value::as_str)
                {
                    if !rc.is_empty() {
                        out.push(StreamEvent::ThinkingDelta {
                            text: rc.to_string(),
                        });
                    }
                }
                if let Some(tcs) = delta
                    .and_then(|d| d.get("tool_calls"))
                    .and_then(Value::as_array)
                {
                    for tc in tcs {
                        let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
                        if let std::collections::hash_map::Entry::Vacant(e) = self.tools.entry(idx)
                        {
                            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                                let name = tc
                                    .get("function")
                                    .and_then(|f| f.get("name"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                e.insert(id.to_string());
                                self.order.push(idx);
                                out.push(StreamEvent::ToolUseStart {
                                    id: id.to_string(),
                                    name,
                                });
                            }
                        }
                        if let (Some(id), Some(args)) = (
                            self.tools.get(&idx),
                            tc.get("function")
                                .and_then(|f| f.get("arguments"))
                                .and_then(Value::as_str),
                        ) {
                            if !args.is_empty() {
                                out.push(StreamEvent::ToolUseInputDelta {
                                    id: id.clone(),
                                    partial_json: args.to_string(),
                                });
                            }
                        }
                    }
                }
                if let Some(fr) = ch.get("finish_reason").and_then(Value::as_str) {
                    self.stop = map_finish(fr);
                }
            }
        }
        if let Some(u) = v.get("usage").and_then(Value::as_object) {
            self.usage.input_tokens = u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
            self.usage.output_tokens = u
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.has_usage = true;
            // Usage NICHT hier emittieren: der usage-Chunk kommt vor [DONE], aber die
            // Ordering-Invariante (provider-api.md) verlangt Usage NACH allen ToolUseStop.
            // → in done() ausgeben.
        }
        out
    }

    /// Beim `[DONE]`-Marker: offene Tool-Calls schließen + `MessageStop`.
    pub fn done(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.stopped {
            return out;
        }
        self.stopped = true;
        for idx in &self.order {
            if let Some(id) = self.tools.get(idx) {
                out.push(StreamEvent::ToolUseStop { id: id.clone() });
            }
        }
        if self.has_usage {
            out.push(StreamEvent::Usage(self.usage));
        }
        out.push(StreamEvent::MessageStop {
            stop_reason: self.stop,
        });
        out
    }
}

fn map_finish(s: &str) -> StopReason {
    match s {
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    }
}

/// Dekodiert einen kompletten OpenAI-SSE-Body zu [`StreamEvent`]s (für Fixture-Tests).
pub fn decode_openai_sse(raw: &[u8]) -> Vec<StreamEvent> {
    let mut dec = SseDecoder::new();
    let mut map = OpenAiMapper::default();
    let mut out = Vec::new();
    let mut feed = |payloads: Vec<String>, out: &mut Vec<StreamEvent>| {
        for payload in payloads {
            if payload.trim() == "[DONE]" {
                out.extend(map.done());
            } else if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                out.extend(map.push(&v));
            }
        }
    };
    let pushed = dec.push(raw);
    feed(pushed, &mut out);
    let finished = dec.finish();
    feed(finished, &mut out);
    // Wie der Live-Pfad (stream-end): falls kein `[DONE]` kam, hier abschließen. Der
    // `stopped`-Guard in done() macht einen zweiten Aufruf zum No-op.
    out.extend(map.done());
    out
}

/// OpenAI-kompatibler Provider.
pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: Option<String>,
    base_url: String,
    dialect: OpenAiDialect,
}

impl OpenAiProvider {
    /// Neuer Provider mit optionalem API-Key und base_url (z. B. lokal für Ollama/vLLM).
    /// Dialekt ist per Default [`OpenAiDialect::OpenAi`]; via [`Self::with_dialect`] änderbar.
    pub fn new(api_key: Option<String>, base_url: impl Into<String>) -> Self {
        OpenAiProvider {
            client: reqwest::Client::new(),
            api_key,
            base_url: base_url.into(),
            dialect: OpenAiDialect::default(),
        }
    }

    /// Setzt den Dialekt (anbieterspezifische Body-Felder). Builder-Stil, damit die `new`-Signatur
    /// stabil bleibt.
    pub fn with_dialect(mut self, dialect: OpenAiDialect) -> Self {
        self.dialect = dialect;
        self
    }

    /// `OPENAI_API_KEY` (optional für lokale Server) + `OPENAI_BASE_URL` (Default OpenAI).
    pub fn from_env() -> Result<Self> {
        let base =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let key = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());
        Ok(Self::new(key, base))
    }

    fn build_body(&self, req: &CompletionRequest) -> Value {
        build_chat_body(req, self.dialect)
    }
}

/// Baut den Chat-Completions-Request-Body für einen OpenAI-kompatiblen Endpunkt. Geteilt vom
/// [`OpenAiProvider`] und dem dedizierten z.ai-Connector ([`crate::zai::ZaiProvider`]) — der
/// einzige anbieterspezifische Unterschied steckt im `dialect`.
pub(crate) fn build_chat_body(req: &CompletionRequest, dialect: OpenAiDialect) -> Value {
    let max = if req.max_tokens > 0 {
        req.max_tokens
    } else {
        req.model.max_output_tokens
    };
    let mut messages: Vec<Value> = Vec::new();
    if let Some(sys) = req.system {
        if !sys.is_empty() {
            messages.push(json!({ "role": "system", "content": sys }));
        }
    }
    messages.extend(req.messages.iter().flat_map(message_to_openai));

    let mut body = json!({
        "model": req.model.id,
        "stream": true,
        "stream_options": { "include_usage": true },
        "max_tokens": max,
        "messages": messages,
    });
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }
    // z.ai/GLM nimmt ein binäres `thinking`-Objekt; echtes OpenAI/local NICHT (würde das
    // unbekannte Feld mit 400 ablehnen) — daher streng auf den Zai-Dialekt + reasoning-fähiges
    // Modell gegated. `Off` → explizit "disabled" (GLM denkt sonst per Default weiter und das
    // kostet ein Vielfaches an completion_tokens); jede andere Stufe → "enabled". GLM ist binär,
    // daher kein Budget (anders als Anthropic, anthropic.rs).
    if dialect == OpenAiDialect::Zai && req.model.supports_reasoning {
        let mode = if req.thinking == ThinkingLevel::Off {
            "disabled"
        } else {
            "enabled"
        };
        body["thinking"] = json!({ "type": mode });
    }
    body
}

/// Mappt eine sepp-`Message` auf 0..n OpenAI-Messages (Tool-Results werden zu eigenen
/// `role:"tool"`-Einträgen).
fn message_to_openai(msg: &Message) -> Vec<Value> {
    let mut out = Vec::new();
    match msg.role {
        Role::System => {
            let text = text_of(&msg.content);
            if !text.is_empty() {
                out.push(json!({ "role": "system", "content": text }));
            }
        }
        Role::User | Role::Tool => {
            // Tool-Results → je eigener role:"tool"-Eintrag.
            for b in &msg.content {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } = b
                {
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": text_of(content),
                    }));
                }
            }
            let text = text_of(&msg.content);
            if !text.is_empty() {
                out.push(json!({ "role": "user", "content": text }));
            }
        }
        Role::Assistant => {
            let text = text_of(&msg.content);
            let tool_calls: Vec<Value> = msg
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => Some(json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": input.to_string() },
                    })),
                    _ => None,
                })
                .collect();
            let mut m = json!({ "role": "assistant" });
            if tool_calls.is_empty() {
                m["content"] = json!(text);
            } else {
                m["content"] = if text.is_empty() {
                    Value::Null
                } else {
                    json!(text)
                };
                m["tool_calls"] = json!(tool_calls);
            }
            out.push(m);
        }
    }
    out
}

fn text_of(blocks: &[ContentBlock]) -> String {
    let mut s = String::new();
    for b in blocks {
        if let ContentBlock::Text { text } = b {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(text);
        }
    }
    s
}

struct DecodeState {
    bytes: BoxStream<'static, reqwest::Result<Bytes>>,
    decoder: SseDecoder,
    mapper: OpenAiMapper,
    pending: std::collections::VecDeque<StreamEvent>,
    finished: bool,
    cancel: CancellationToken,
    /// Anbieter-Label für Fehlertexte (`"openai"` bzw. `"zai"`), damit ein z.ai-Fehler nicht
    /// fälschlich als OpenAI-Fehler erscheint.
    label: &'static str,
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    async fn stream<'a>(
        &'a self,
        req: CompletionRequest<'a>,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'a, StreamEvent>> {
        let body = self.build_body(&req);
        stream_chat(
            &self.client,
            &self.base_url,
            self.api_key.as_deref(),
            body,
            "openai",
            cancel,
        )
        .await
    }
}

/// Führt einen Chat-Completions-Stream gegen einen OpenAI-kompatiblen Endpunkt aus: POST mit
/// optionalem Bearer, Statusprüfung, dann SSE → [`StreamEvent`]. Geteilt vom [`OpenAiProvider`]
/// und dem dedizierten z.ai-Connector ([`crate::zai::ZaiProvider`]). `label` taucht in allen
/// Fehlertexten auf, damit ein Anbieter nicht unter dem Namen des anderen scheitert. Der Body
/// hält die Antwort am Leben, deshalb ist der zurückgegebene Stream `'static`.
pub(crate) async fn stream_chat(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    body: Value,
    label: &'static str,
    cancel: CancellationToken,
) -> Result<BoxStream<'static, StreamEvent>> {
    let url = format!("{base_url}/chat/completions");
    let mut builder = client.post(&url).json(&body);
    if let Some(key) = api_key {
        builder = builder.bearer_auth(key);
    }
    let resp = builder
        .send()
        .await
        .map_err(|e| SeppError::Provider(format!("{label} request: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(SeppError::Provider(format!(
            "{label}: HTTP {status}: {}",
            text.trim()
        )));
    }

    let state = DecodeState {
        bytes: Box::pin(resp.bytes_stream()),
        decoder: SseDecoder::new(),
        mapper: OpenAiMapper::default(),
        pending: std::collections::VecDeque::new(),
        finished: false,
        cancel,
        label,
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
                    Some(Ok(b)) => feed(&mut st, &b),
                    Some(Err(e)) => {
                        st.pending.push_back(StreamEvent::Error { message: format!("{} stream: {e}", st.label) });
                        st.finished = true;
                    }
                    None => {
                        let rest = st.decoder.finish();
                        for p in rest { dispatch(&mut st, &p); }
                        for ev in st.mapper.done() { st.pending.push_back(ev); }
                        st.finished = true;
                    }
                }
            }
        }
    });
    Ok(Box::pin(stream))
}

fn feed(st: &mut DecodeState, bytes: &[u8]) {
    for payload in st.decoder.push(bytes) {
        dispatch(st, &payload);
    }
}

fn dispatch(st: &mut DecodeState, payload: &str) {
    if payload.trim() == "[DONE]" {
        for ev in st.mapper.done() {
            st.pending.push_back(ev);
        }
    } else if let Ok(v) = serde_json::from_str::<Value>(payload) {
        for ev in st.mapper.push(&v) {
            st.pending.push_back(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_text_and_tool_call_stream() {
        let raw = include_bytes!("../tests/fixtures/openai_basic.sse");
        let events = decode_openai_sse(raw);
        assert!(matches!(events.first(), Some(StreamEvent::MessageStart)));
        assert!(matches!(
            events.last(),
            Some(StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse
            })
        ));
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hallo Welt");
        assert!(events.iter().any(|e| matches!(e,
            StreamEvent::ToolUseStart { id, name } if id == "call_1" && name == "get_weather")));
        let args: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseInputDelta { partial_json, .. } => Some(partial_json.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(args, "{\"city\":\"Berlin\"}");
        let usage = events.iter().find_map(|e| match e {
            StreamEvent::Usage(u) => Some(*u),
            _ => None,
        });
        assert_eq!(usage.map(|u| u.output_tokens), Some(7));

        // Ordering-Invariante: ToolUseStop vor Usage vor MessageStop.
        let stop = events
            .iter()
            .position(|e| matches!(e, StreamEvent::ToolUseStop { .. }))
            .unwrap();
        let usage_pos = events
            .iter()
            .position(|e| matches!(e, StreamEvent::Usage(_)))
            .unwrap();
        let msgstop = events
            .iter()
            .position(|e| matches!(e, StreamEvent::MessageStop { .. }))
            .unwrap();
        assert!(stop < usage_pos && usage_pos < msgstop, "{events:?}");
    }

    #[test]
    fn stream_without_done_marker_still_terminates() {
        // Manche OpenAI-kompatible Server (Ollama/vLLM) senden kein `data: [DONE]`.
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n"
        );
        let ev = decode_openai_sse(raw.as_bytes());
        assert!(
            matches!(
                ev.last(),
                Some(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn
                })
            ),
            "{ev:?}"
        );
        // genau ein MessageStop trotz fehlendem [DONE]
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e, StreamEvent::MessageStop { .. }))
                .count(),
            1
        );
    }

    fn test_model(reasoning: bool) -> sepp_core::Model {
        sepp_core::Model {
            id: "glm-5.2".into(),
            provider: "zai".into(),
            display_name: "GLM-5.2".into(),
            context_window: 200_000,
            max_output_tokens: 32_000,
            supports_reasoning: reasoning,
            supports_images: false,
        }
    }

    fn test_req(model: &sepp_core::Model, thinking: ThinkingLevel) -> CompletionRequest<'_> {
        CompletionRequest {
            model,
            system: None,
            messages: &[],
            tools: &[],
            thinking,
            max_tokens: 8192,
        }
    }

    #[test]
    fn zai_thinking_enabled_for_nonoff_level() {
        let p = OpenAiProvider::new(None, "x").with_dialect(OpenAiDialect::Zai);
        let m = test_model(true);
        let body = p.build_body(&test_req(&m, ThinkingLevel::Medium));
        assert_eq!(body["thinking"], json!({ "type": "enabled" }));
    }

    #[test]
    fn zai_thinking_disabled_for_off() {
        let p = OpenAiProvider::new(None, "x").with_dialect(OpenAiDialect::Zai);
        let m = test_model(true);
        let body = p.build_body(&test_req(&m, ThinkingLevel::Off));
        assert_eq!(body["thinking"], json!({ "type": "disabled" }));
    }

    #[test]
    fn zai_thinking_absent_when_model_unsupported() {
        let p = OpenAiProvider::new(None, "x").with_dialect(OpenAiDialect::Zai);
        let m = test_model(false);
        let body = p.build_body(&test_req(&m, ThinkingLevel::Medium));
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn openai_dialect_never_emits_thinking() {
        // Default-Dialekt (echtes OpenAI/local) darf das Feld nie senden — würde 400en.
        let p = OpenAiProvider::new(None, "x");
        let m = test_model(true);
        let body = p.build_body(&test_req(&m, ThinkingLevel::Medium));
        assert!(body.get("thinking").is_none());
    }
}
