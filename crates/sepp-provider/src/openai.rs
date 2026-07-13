//! OpenAI-kompatibler Adapter (Chat-Completions, SSE). Über die `base_url` deckt er auch
//! lokale Endpunkte ab (Ollama/vLLM, OpenAI-kompatibel). Auth aus `OPENAI_API_KEY` (optional
//! für lokale Server). Feature `openai`.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use sepp_core::{ContentBlock, Message, Result, Role, SeppError, ThinkingLevel, Usage};

use crate::sse::SseDecoder;
use crate::{CompletionRequest, Provider, StopReason, StreamEvent};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// Host:Port des LM-Studio-Default-Servers — die eine Quelle für den CLI-Preflight und die
/// Meldungstexte des `mlx`-Presets. Konsistenz zu [`MLX_BASE_URL`] sichert ein Unit-Test.
pub const MLX_HOST_PORT: &str = "localhost:1234";
/// Default-Endpunkt des `mlx`-Presets: LM Studios lokaler OpenAI-kompatibler Server (Port 1234).
/// Über `OPENAI_BASE_URL` überschreibbar (abweichender Host/Port).
pub const MLX_BASE_URL: &str = "http://localhost:1234/v1";

/// Welcher OpenAI-kompatible Dialekt gesprochen wird. Steuert anbieterspezifische Body-Felder:
/// z. B. das z.ai-`thinking`-Objekt, das echtes OpenAI als unbekanntes Feld mit HTTP 400 ablehnen
/// würde. Default ist [`OpenAiDialect::OpenAi`]; `--provider local` setzt
/// [`OpenAiDialect::Local`], der dedizierte z.ai-Connector ([`crate::zai::ZaiProvider`])
/// [`OpenAiDialect::Zai`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpenAiDialect {
    /// Striktes OpenAI/Chat-Completions (OpenAI, LM Studio via mlx, …) — keine Zusatzfelder.
    #[default]
    OpenAi,
    /// Lokaler Endpunkt via `--provider local` (Ollama/vLLM): sendet bei `ThinkingLevel::Off`
    /// `reasoning_effort:"none"`. Ollama aktiviert Thinking für fähige Modelle sonst per
    /// Server-Default auch über /v1 — die finale Antwort landet dann teils komplett im
    /// `reasoning`-Feld und `content` (stdout) bleibt leer. Echtes OpenAI bekommt das Feld
    /// bewusst NICHT (ältere o-Modelle lehnen den Wert "none" mit HTTP 400 ab).
    Local,
    /// z.ai / Zhipu-GLM — akzeptiert zusätzlich `thinking:{type:…}`.
    Zai,
}

/// Übersetzt OpenAI-SSE-Deltas (zustandsbehaftet) in [`StreamEvent`].
#[derive(Debug, Default)]
pub struct OpenAiMapper {
    started: bool,
    stopped: bool,
    tools: HashMap<u64, String>, // index -> id des aktuell offenen Tool-Calls
    /// Alle gestarteten Tool-Call-ids in Startreihenfolge — Grundlage für die Stops in `done()`.
    /// Invariante: genau EIN Eintrag (und ein `ToolUseStart`) je id, auch bei Index-Recycling
    /// oder Index-Drift — sonst entstünden doppelte PendingCalls mit derselben tool_call_id.
    started_ids: Vec<String>,
    /// ids, für die bereits ein `ToolUseStop` emittiert wurde (Index-Recycling schließt früh).
    closed_ids: HashSet<String>,
    /// Zähler für synthetische ids (`call_synth_{n}`), wenn ein Server einen Tool-Call mit
    /// leerer/fehlender id eröffnet — der Call soll laufen statt stumm verworfen zu werden.
    synth_seq: u64,
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
                // Reasoning-Modelle über OpenAI-kompatible Endpunkte streamen ihr Denken in
                // `reasoning_content` (z. B. z.ai GLM-4.6, DeepSeek-R1) oder `reasoning`
                // (Ollama) statt `content`. Als ThinkingDelta abbilden statt verwerfen; für
                // reine Chat-Modelle (kein solches Feld) ist der Zweig ein No-op.
                if let Some(rc) = delta
                    .and_then(|d| d.get("reasoning_content").or_else(|| d.get("reasoning")))
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
                        // Leere ids wie „keine id" behandeln (degenerierte Server).
                        let chunk_id = tc
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty());
                        match chunk_id {
                            // Bereits GESCHLOSSENE id taucht erneut auf (A→B→A am selben Index):
                            // degenerierter Server — Chunk samt Argumenten ignorieren. Ein zweiter
                            // Start ohne zweites Stop bräche die Invariante „genau ein Start/Stop
                            // je id" (doppelte tool_use-Blöcke im Folge-Request → HTTP 400).
                            Some(id) if self.closed_ids.contains(id) => continue,
                            // id ist bereits OFFEN, ggf. unter neuem Index (Index-Drift) oder
                            // erneut im Folge-Chunk (Server wiederholen sie teils pro Chunk):
                            // nur das Index-Mapping nachziehen, KEIN zweiter Start. Ein dabei
                            // verdrängter anderer offener Call bleibt in `started_ids` und wird
                            // in done() geschlossen.
                            Some(id) if self.started_ids.iter().any(|s| s.as_str() == id) => {
                                self.tools.insert(idx, id.to_string());
                            }
                            // Neue id: Manche Server (llama.cpp-Familie, LM Studio) recyceln
                            // index 0 für JEDEN Tool-Call — den vorigen Call schließen, sonst
                            // würden die Argumente beider Calls unter der ersten id konkateniert.
                            Some(id) => {
                                if let Some(old) = self.tools.insert(idx, id.to_string()) {
                                    if self.closed_ids.insert(old.clone()) {
                                        out.push(StreamEvent::ToolUseStop { id: old });
                                    }
                                }
                                self.started_ids.push(id.to_string());
                                out.push(StreamEvent::ToolUseStart {
                                    id: id.to_string(),
                                    name: tool_name(tc),
                                });
                            }
                            // Keine/leere id: auf offenem Index Continuation des laufenden Calls
                            // (args-Folge-Chunks tragen keine id). Auf freiem Index eröffnet der
                            // Chunk einen Call OHNE id (degenerierte Server) — synthetische id
                            // vergeben statt den Call stumm zu verwerfen: tool_use.id und
                            // tool_result.tool_use_id stammen beide aus demselben PendingCall,
                            // die Paarung bleibt also auch mit erfundener id konsistent.
                            None => {
                                if let Entry::Vacant(e) = self.tools.entry(idx) {
                                    let id = format!("call_synth_{}", self.synth_seq);
                                    self.synth_seq += 1;
                                    e.insert(id.clone());
                                    self.started_ids.push(id.clone());
                                    out.push(StreamEvent::ToolUseStart {
                                        id,
                                        name: tool_name(tc),
                                    });
                                }
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
        for id in &self.started_ids {
            // Dedupe: bei Index-Recycling früh geschlossene Calls nicht erneut stoppen.
            if self.closed_ids.insert(id.clone()) {
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

/// `function.name` aus einem tool_call-Delta — kommt nur im ersten Chunk eines Calls mit;
/// Folge-Chunks tragen nur `arguments`, dann bleibt es leer.
fn tool_name(tc: &Value) -> String {
    tc.get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
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

/// Nicht-leerer, getrimmter Wert oder `None` — DIE Semantik für „Env-Wert vorhanden":
/// leer/Whitespace zählt als fehlend, umgebender Whitespace (Copy-Paste, Shell-Profile) wird
/// entfernt. Ein Trailing Space in einer base_url würde sonst als `%20` in der Request-URL
/// landen (404), ein Whitespace-Key als sinnloser `Bearer`-Header. `pub`, damit die
/// CLI-Frühchecks (sepp-cli) exakt dieselbe Auflösung nutzen wie die Provider.
pub fn nonempty_trimmed(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Effektive base_url: nicht-leerer Env-Wert gewinnt (getrimmt), sonst `default`.
/// Leere/Whitespace-Werte (z. B. `OPENAI_BASE_URL=""` aus Shell-Profilen/CI) zählen als
/// „nicht gesetzt" — sonst ginge der Preset-Default verloren und der erste Request scheitert
/// an einer relativen URL.
pub(crate) fn resolve_base_url(env_val: Option<String>, default: &str) -> String {
    nonempty_trimmed(env_val).unwrap_or_else(|| default.to_string())
}

/// OpenAI-kompatibler Provider.
pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: Option<String>,
    base_url: String,
    dialect: OpenAiDialect,
    /// Anbieter-Label für `name()` und alle Fehlertexte (`"openai"` bzw. `"mlx"`) — ein
    /// LM-Studio-Fehler darf nicht als OpenAI-Fehler erscheinen (vgl. [`DecodeState`]).
    label: &'static str,
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
            label: "openai",
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
        let base = resolve_base_url(std::env::var("OPENAI_BASE_URL").ok(), DEFAULT_BASE_URL);
        let key = nonempty_trimmed(std::env::var("OPENAI_API_KEY").ok());
        Ok(Self::new(key, base))
    }

    /// `mlx`-Preset (`--provider mlx`): lokaler LM-Studio-Server. base_url aus `OPENAI_BASE_URL`
    /// (Default [`MLX_BASE_URL`]), fällt bewusst NICHT auf api.openai.com zurück wie
    /// [`Self::from_env`] — so verbindet `sepp --provider mlx` ohne Env-Konfiguration direkt zu
    /// LM Studio. `OPENAI_API_KEY` wird nur mitgesendet, wenn `OPENAI_BASE_URL` explizit gesetzt
    /// ist (bewusstes Opt-in, siehe [`mlx_config`]); Fehler melden sich als „mlx", nicht „openai".
    pub fn mlx_from_env() -> Result<Self> {
        let (base, key) = mlx_config(
            std::env::var("OPENAI_BASE_URL").ok(),
            std::env::var("OPENAI_API_KEY").ok(),
        );
        let mut p = Self::new(key, base);
        p.label = "mlx";
        Ok(p)
    }

    fn build_body(&self, req: &CompletionRequest) -> Value {
        build_chat_body(req, self.dialect)
    }
}

/// mlx-Konfiguration aus (bereits gelesenen) Env-Werten: base_url mit LM-Studio-Default; der
/// API-Key wird NUR übernommen, wenn der Endpunkt explizit gesetzt ist. Im Zero-Config-Fall
/// (localhost:1234) darf ein für andere Tools exportierter echter `OPENAI_API_KEY` nicht als
/// Bearer über Klartext-HTTP an einen beliebigen lokalen Prozess auf Port 1234 lecken.
fn mlx_config(env_base: Option<String>, env_key: Option<String>) -> (String, Option<String>) {
    // Key-Opt-in und Default-Endpunkt hängen an EINER Auflösung — zwei getrennte Prädikate
    // über denselben Wert könnten auseinanderdriften (Key ginge dann an den falschen Endpunkt).
    let base_override = nonempty_trimmed(env_base);
    let key = if base_override.is_some() {
        nonempty_trimmed(env_key)
    } else {
        None
    };
    let base = base_override.unwrap_or_else(|| MLX_BASE_URL.to_string());
    (base, key)
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
    // Abschaltung des Server-Default-Thinkings lokaler Endpunkte — Begründung und Gating
    // siehe [`OpenAiDialect::Local`].
    if dialect == OpenAiDialect::Local && req.thinking == ThinkingLevel::Off {
        body["reasoning_effort"] = json!("none");
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
        self.label
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
            self.label,
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
    let resp = builder.send().await.map_err(|e| {
        // Verbindungsfehler nennen den Endpunkt: hilfreicher bei lokalen Servern (mlx/local),
        // die zwischen Preflight und Request sterben können, statt eines rohen reqwest-Texts.
        if e.is_connect() {
            SeppError::Provider(format!(
                "{label}: Verbindung zu {base_url} fehlgeschlagen: {e} — läuft der Server?"
            ))
        } else {
            SeppError::Provider(format!("{label} request: {e}"))
        }
    })?;
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
    fn decodes_ollama_reasoning_stream() {
        // Aufgezeichnetes Ollama-/v1-Drahtformat (0.31.x, Thinking per Server-Default an):
        // das Denken kommt im Delta-Feld `reasoning` (nicht `reasoning_content`), daneben
        // steht ein leerer `content`-String — der darf kein TextDelta erzeugen.
        let raw = include_bytes!("../tests/fixtures/ollama_reasoning.sse");
        let events = decode_openai_sse(raw);
        assert!(matches!(events.first(), Some(StreamEvent::MessageStart)));
        assert!(matches!(
            events.last(),
            Some(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn
            })
        ));
        let thinking: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ThinkingDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "Die Antwort muss knapp sein.");
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hallo von Ollama");
        let usage = events.iter().find_map(|e| match e {
            StreamEvent::Usage(u) => Some(*u),
            _ => None,
        });
        assert_eq!(usage.map(|u| u.output_tokens), Some(24));
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
    fn local_dialect_disables_reasoning_when_thinking_off() {
        let p = OpenAiProvider::new(None, "x").with_dialect(OpenAiDialect::Local);
        let m = test_model(true);
        let body = p.build_body(&test_req(&m, ThinkingLevel::Off));
        assert_eq!(body["reasoning_effort"], json!("none"));
    }

    #[test]
    fn local_dialect_leaves_reasoning_untouched_when_thinking_on() {
        let p = OpenAiProvider::new(None, "x").with_dialect(OpenAiDialect::Local);
        let m = test_model(true);
        let body = p.build_body(&test_req(&m, ThinkingLevel::Medium));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn openai_dialect_never_emits_reasoning_effort() {
        // Echtes OpenAI darf das Feld nie bekommen — ältere o-Modelle 400en auf "none".
        let p = OpenAiProvider::new(None, "x");
        let m = test_model(true);
        let body = p.build_body(&test_req(&m, ThinkingLevel::Off));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn openai_dialect_never_emits_thinking() {
        // Default-Dialekt (echtes OpenAI/local) darf das Feld nie senden — würde 400en.
        let p = OpenAiProvider::new(None, "x");
        let m = test_model(true);
        let body = p.build_body(&test_req(&m, ThinkingLevel::Medium));
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn resolve_base_url_prefers_nonempty_env() {
        // Pure Auflösung statt env-mutierender Tests (remove_var raced im parallelen
        // Test-Binary): None und Leer-/Whitespace-Werte fallen auf den Default.
        assert_eq!(resolve_base_url(None, DEFAULT_BASE_URL), DEFAULT_BASE_URL);
        assert_eq!(
            resolve_base_url(Some(String::new()), MLX_BASE_URL),
            MLX_BASE_URL
        );
        assert_eq!(
            resolve_base_url(Some("  ".into()), MLX_BASE_URL),
            MLX_BASE_URL
        );
        assert_eq!(
            resolve_base_url(Some("http://10.0.0.2:8080/v1".into()), MLX_BASE_URL),
            "http://10.0.0.2:8080/v1"
        );
        // Umgebender Whitespace (Copy-Paste) wird entfernt — ein Trailing Space würde sonst
        // als %20 in der Request-URL landen und der Server antwortete 404.
        assert_eq!(
            resolve_base_url(Some("http://10.0.0.2:8080/v1 ".into()), MLX_BASE_URL),
            "http://10.0.0.2:8080/v1"
        );
    }

    #[test]
    fn nonempty_trimmed_semantics() {
        // DIE eine „Env-Wert vorhanden"-Semantik: leer/Whitespace = fehlend, sonst getrimmt.
        assert_eq!(nonempty_trimmed(None), None);
        assert_eq!(nonempty_trimmed(Some(String::new())), None);
        assert_eq!(nonempty_trimmed(Some("  \n".into())), None);
        assert_eq!(nonempty_trimmed(Some(" x ".into())).as_deref(), Some("x"));
    }

    #[test]
    fn mlx_config_defaults_to_lm_studio_and_withholds_key() {
        // Zero-Config: localhost-Default (NICHT api.openai.com — vermeidet die 401-Klasse)
        // UND kein Bearer — ein für andere Tools exportierter OPENAI_API_KEY darf nicht an
        // einen beliebigen Port-1234-Prozess gehen.
        let (base, key) = mlx_config(None, Some("sk-secret".into()));
        assert_eq!(base, MLX_BASE_URL);
        assert_ne!(base, DEFAULT_BASE_URL);
        assert_eq!(key, None);
    }

    #[test]
    fn mlx_config_sends_key_only_with_explicit_endpoint() {
        let (base, key) = mlx_config(Some("http://host:8080/v1".into()), Some("k".into()));
        assert_eq!(base, "http://host:8080/v1");
        assert_eq!(key.as_deref(), Some("k"));
        // Leerer Override zählt als „nicht gesetzt": Default-Endpunkt, kein Key.
        let (base, key) = mlx_config(Some(String::new()), Some("k".into()));
        assert_eq!(base, MLX_BASE_URL);
        assert_eq!(key, None);
        // Whitespace an base und Key wird getrimmt; Whitespace-only-Key zählt als fehlend.
        let (base, key) = mlx_config(Some(" http://host:8080/v1 ".into()), Some(" k\n".into()));
        assert_eq!(base, "http://host:8080/v1");
        assert_eq!(key.as_deref(), Some("k"));
        let (_, key) = mlx_config(Some("http://host:8080/v1".into()), Some("  ".into()));
        assert_eq!(key, None);
    }

    #[test]
    fn mlx_preset_reports_as_mlx_not_openai() {
        // Der Kern der Label-Trennung: ein LM-Studio-Fehler darf nicht als „openai" erscheinen.
        let p = OpenAiProvider::mlx_from_env().expect("mlx_from_env");
        assert_eq!(p.name(), "mlx");
        assert_eq!(OpenAiProvider::new(None, "x").name(), "openai");
    }

    #[test]
    fn mlx_endpoint_constants_stay_consistent() {
        // MLX_HOST_PORT (CLI-Preflight) und MLX_BASE_URL (Provider) müssen dieselbe Adresse
        // beschreiben — Drift ließe den Preflight gegen den falschen Port prüfen.
        assert_eq!(MLX_BASE_URL, format!("http://{MLX_HOST_PORT}/v1"));
    }

    #[test]
    fn splits_tool_calls_when_server_recycles_index_zero() {
        // llama.cpp-Familie (LM Studio): jeder Tool-Call kommt erneut unter index 0 mit
        // neuer id — der Mapper muss trennen statt die Argumente zu konkatenieren.
        let raw = include_bytes!("../tests/fixtures/openai_repeated_index.sse");
        let events = decode_openai_sse(raw);
        let starts: Vec<(&str, &str)> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseStart { id, name } => Some((id.as_str(), name.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(starts, [("call_1", "get_weather"), ("call_2", "get_time")]);
        let args_for = |id: &str| -> String {
            events
                .iter()
                .filter_map(|e| match e {
                    StreamEvent::ToolUseInputDelta {
                        id: i,
                        partial_json,
                    } if i == id => Some(partial_json.as_str()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(args_for("call_1"), "{\"city\":\"Berlin\"}");
        assert_eq!(args_for("call_2"), "{\"tz\":\"UTC\"}");
        // Stop(call_1) VOR Start(call_2); genau ein Stop je id, in Startreihenfolge.
        let stop1 = events
            .iter()
            .position(|e| matches!(e, StreamEvent::ToolUseStop { id } if id == "call_1"))
            .unwrap();
        let start2 = events
            .iter()
            .position(|e| matches!(e, StreamEvent::ToolUseStart { id, .. } if id == "call_2"))
            .unwrap();
        assert!(stop1 < start2, "{events:?}");
        let stops: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseStop { id } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(stops, ["call_1", "call_2"]);
        // Ordering-Invariante bleibt: letzter Stop < Usage < MessageStop.
        let last_stop = events
            .iter()
            .rposition(|e| matches!(e, StreamEvent::ToolUseStop { .. }))
            .unwrap();
        let usage_pos = events
            .iter()
            .position(|e| matches!(e, StreamEvent::Usage(_)))
            .unwrap();
        let msgstop = events
            .iter()
            .position(|e| matches!(e, StreamEvent::MessageStop { .. }))
            .unwrap();
        assert!(last_stop < usage_pos && usage_pos < msgstop, "{events:?}");
    }

    #[test]
    fn repeated_same_id_chunks_do_not_restart_tool_call() {
        // Manche Server wiederholen die id in JEDEM Chunk — das darf keinen neuen Call starten.
        let mut m = OpenAiMapper::default();
        let c1 = json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call_1","function":{"name":"f","arguments":"{\"a\":"}}]}}]});
        let c2 = json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call_1","function":{"arguments":"1}"}}]}}]});
        let mut ev = m.push(&c1);
        ev.extend(m.push(&c2));
        ev.extend(m.done());
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e, StreamEvent::ToolUseStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e, StreamEvent::ToolUseStop { .. }))
                .count(),
            1
        );
        let args: String = ev
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseInputDelta { partial_json, .. } => Some(partial_json.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(args, "{\"a\":1}");
    }

    #[test]
    fn empty_tool_call_id_gets_synthetic_id() {
        // Leere id im ERSTEN Chunk: der Call läuft mit synthetischer id durch (wie unter
        // 0.1.11, wo er mit id "" lief) statt stumm verworfen zu werden; Folge-Chunks mit
        // leerer id sind Continuation desselben Calls.
        let mut m = OpenAiMapper::default();
        let c1 = json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"","function":{"name":"f","arguments":"{\"a\":"}}]}}]});
        let c2 = json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"1}"}}]}}]});
        let mut ev = m.push(&c1);
        ev.extend(m.push(&c2));
        ev.extend(m.done());
        let starts: Vec<&str> = ev
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseStart { id, name } => {
                    assert_eq!(name, "f");
                    Some(id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 1, "{ev:?}");
        assert!(starts[0].starts_with("call_synth_"), "{}", starts[0]);
        let args: String = ev
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseInputDelta { id, partial_json } => {
                    assert_eq!(id, starts[0]);
                    Some(partial_json.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(args, "{\"a\":1}");
        let stops: Vec<&str> = ev
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseStop { id } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(stops, [starts[0]]);
    }

    #[test]
    fn reappearing_closed_id_is_ignored() {
        // A→B→A am selben Index: die bereits geschlossene id A darf weder erneut starten noch
        // Argumente annehmen — genau ein Start/Stop je id, in Startreihenfolge.
        let mut m = OpenAiMapper::default();
        let a1 = json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call_a","function":{"name":"fa","arguments":"{\"x\":1}"}}]}}]});
        let b = json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call_b","function":{"name":"fb","arguments":"{}"}}]}}]});
        let a2 = json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call_a","function":{"arguments":"{\"y\":2}"}}]}}]});
        let mut ev = m.push(&a1);
        ev.extend(m.push(&b));
        ev.extend(m.push(&a2));
        ev.extend(m.done());
        let starts: Vec<&str> = ev
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseStart { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(starts, ["call_a", "call_b"], "{ev:?}");
        let stops: Vec<&str> = ev
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseStop { id } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(stops, ["call_a", "call_b"], "{ev:?}");
        // Die Argumente des dritten Chunks (wiederauferstandenes call_a) werden verworfen.
        let a_args: String = ev
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseInputDelta { id, partial_json } if id == "call_a" => {
                    Some(partial_json.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(a_args, "{\"x\":1}");
    }

    #[test]
    fn same_id_under_new_index_does_not_restart() {
        // Index-Drift: dieselbe id taucht unter neuem Index auf — Continuation, kein zweiter
        // Start; die Argumente beider Chunks konkatenzieren unter der einen id.
        let mut m = OpenAiMapper::default();
        let c1 = json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call_a","function":{"name":"f","arguments":"{\"a\":"}}]}}]});
        let c2 = json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":1,"id":"call_a","function":{"arguments":"1}"}}]}}]});
        let mut ev = m.push(&c1);
        ev.extend(m.push(&c2));
        ev.extend(m.done());
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e, StreamEvent::ToolUseStart { .. }))
                .count(),
            1,
            "{ev:?}"
        );
        let args: String = ev
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseInputDelta { partial_json, .. } => Some(partial_json.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(args, "{\"a\":1}");
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e, StreamEvent::ToolUseStop { .. }))
                .count(),
            1,
            "{ev:?}"
        );
    }
}
