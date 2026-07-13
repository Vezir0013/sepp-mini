//! `sepp-agent` — der Agent-Loop und die Orchestrierung.
//!
//! Ablauf eines `prompt`: Provider streamen → Tool-Calls sammeln → **parallel** ausführen →
//! Ergebnisse (getrunkt durch die Tools) zurückspeisen → wiederholen, bis keine Tool-Calls
//! mehr kommen. Optional persistiert ein [`SessionStore`] jeden Schritt; ein Kontext-Budget
//! löst bei Schwellüberschreitung automatische Compaction aus.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use sepp_core::{ContentBlock, Message, Model, Result, Role, SeppError, ThinkingLevel, Usage};
use sepp_hooks::{HookEvent, HookHost, HookOutcome};
use sepp_provider::{CompletionRequest, Provider, StreamEvent};
use sepp_session::{summary_message, EntryPayload, SessionStore};
use sepp_tools::Tool;

pub mod resources;
pub mod subagent;
pub use subagent::SubAgentTool;

/// Öffentlich beobachtbarer Zustand einer Session.
pub struct AgentState {
    pub messages: Vec<Message>,
    pub model: Model,
    pub system_prompt: String,
    pub thinking: ThinkingLevel,
    pub tools: Vec<Arc<dyn Tool>>,
}

/// Ereignisse, die der Loop nach außen meldet (für TUI/Print-Frontends).
#[derive(Debug, Clone)]
pub enum AgentEvent {
    TurnStart,
    TextDelta(String),
    ThinkingDelta(String),
    ToolStart { id: String, name: String },
    ToolEnd { id: String, is_error: bool },
    TurnEnd,
    Done,
    Error(String),
}

/// Ein während des Streams aufgesammelter Tool-Call.
struct PendingCall {
    id: String,
    name: String,
    input_json: String,
}

/// Default-Schwelle der Auto-Compaction für ein Modell: 3/4 des Kontextfensters (geschätzte
/// Tokens) — lieber früher komprimieren als überlaufen. DIE eine Formel für den Start
/// (`sepp-cli`) und Modellwechsel ([`AgentSession::set_model`]); zwei Kopien würden driften.
pub fn default_compact_threshold(model: &Model) -> u64 {
    model.context_window.saturating_mul(3) / 4
}

/// Eine laufende Agent-Session.
pub struct AgentSession {
    provider: Arc<dyn Provider>,
    state: AgentState,
    tools_by_name: HashMap<String, Arc<dyn Tool>>,
    max_tokens: u64,
    max_turns: usize,
    session: Option<Box<dyn SessionStore>>,
    auto_compact_threshold: Option<u64>,
    last_usage: Option<Usage>,
    hooks: Option<Box<dyn HookHost>>,
    hooks_started: bool,
}

impl AgentSession {
    /// Builder.
    pub fn builder() -> AgentSessionBuilder {
        AgentSessionBuilder::default()
    }

    /// Lesezugriff auf den Zustand.
    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Aktuelle Conversation (Messages).
    pub fn messages(&self) -> &[Message] {
        &self.state.messages
    }

    /// Aktives Modell.
    pub fn model(&self) -> &Model {
        &self.state.model
    }

    /// Geschätzte Token-Auslastung des aktiven Kontexts.
    pub fn estimated_tokens(&self) -> u64 {
        let est = estimate_tokens(&self.state.messages);
        match self.last_usage {
            Some(u) => est.max(u.input_tokens + u.output_tokens),
            None => est,
        }
    }

    /// Schließt die Session ab: macht den Store durabel (fsync). Idempotent — mehrfaches Aufrufen
    /// flusht erneut. Frontends rufen dies beim Ende der Konversation auf (One-shot nach dem Prompt,
    /// RPC beim Shutdown, TUI beim Quit).
    pub async fn finalize(&mut self) -> Result<()> {
        self.flush_session().await
    }

    /// Lesezugriff auf den Session-Store (für `/tree` etc.).
    pub fn session(&self) -> Option<&dyn SessionStore> {
        self.session.as_deref()
    }

    /// Schreibzugriff auf den Session-Store (für `branch`/`set_label`).
    pub fn session_mut(&mut self) -> Option<&mut (dyn SessionStore + 'static)> {
        self.session.as_deref_mut()
    }

    /// Ersetzt den Session-Store und lädt dessen aktive Conversation (für `/new`, `/resume`).
    /// Setzt das Kontext-Budget (`last_usage`) für die neue Session zurück.
    pub fn set_session(&mut self, store: Box<dyn SessionStore>) {
        self.state.messages = store.path_messages();
        self.last_usage = None;
        self.session = Some(store);
    }

    /// Ersetzt die Hooks (für `/reload`). Setzt das `before_agent_start`-Flag zurück.
    pub fn set_hooks(&mut self, hooks: Option<Box<dyn HookHost>>) {
        self.hooks = hooks;
        self.hooks_started = false;
    }

    /// Setzt den System-Prompt (z. B. nach erneutem Laden von Skills/Resources).
    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.state.system_prompt = prompt.into();
    }

    /// Lädt die aktive Conversation aus dem Session-Store neu (z. B. nach `branch`).
    pub fn reload_from_session(&mut self) {
        if let Some(s) = self.session.as_ref() {
            self.state.messages = s.path_messages();
        }
    }

    /// Wechselt das Modell für folgende Turns. Zieht die Auto-Compaction-Schwelle auf den
    /// Default des neuen Modells nach, sofern Auto-Compaction aktiv ist — sonst bliebe die
    /// beim Start eingefrorene Schwelle stehen und ein kleineres Kontextfenster liefe über,
    /// bevor je komprimiert wird. `None` bleibt `None` (bewusst deaktiviert, z. B. Sub-Agenten).
    pub fn set_model(&mut self, model: Model) {
        if self.auto_compact_threshold.is_some() {
            self.auto_compact_threshold = Some(default_compact_threshold(&model));
        }
        self.state.model = model;
    }

    /// Setzt die Reasoning-Stufe für folgende Turns (TUI-`/think`). Greift ab dem nächsten
    /// Request — der Loop liest `state.thinking` bei jedem Turn neu.
    pub fn set_thinking(&mut self, level: ThinkingLevel) {
        self.state.thinking = level;
    }

    /// Aktuelle Auto-Compaction-Schwelle (geschätzte Tokens); `None` = deaktiviert.
    pub fn auto_compact_threshold(&self) -> Option<u64> {
        self.auto_compact_threshold
    }

    /// Name des verdrahteten Providers (z. B. `"anthropic"`, `"openai"`, `"mlx"`, `"zai"`) —
    /// für Frontends, die provider-bewusste Entscheidungen treffen (z. B. Custom-Modelle beim
    /// TUI-`/model`), ohne das Provider-Feld selbst zu exponieren.
    pub fn provider_name(&self) -> &str {
        self.provider.name()
    }

    fn record(&mut self, payload: EntryPayload) -> Result<()> {
        if let Some(s) = self.session.as_mut() {
            s.append(payload)?;
        }
        Ok(())
    }

    async fn flush_session(&mut self) -> Result<()> {
        if let Some(s) = self.session.as_mut() {
            s.flush().await?;
        }
        Ok(())
    }

    fn should_compact(&self) -> bool {
        match self.auto_compact_threshold {
            Some(thr) => self.estimated_tokens() > thr,
            None => false,
        }
    }

    /// Verdichtet die bisherige Conversation zu einer Zusammenfassung (Phase-2-Compaction).
    /// Persistiert einen `Compaction`-Eintrag (falls ein Store vorhanden ist) und ersetzt die
    /// In-Memory-Messages durch die Zusammenfassung.
    pub async fn compact(&mut self, instructions: Option<&str>) -> Result<()> {
        if self.state.messages.is_empty() {
            return Ok(());
        }
        let instr = instructions.unwrap_or(
            "Fasse das bisherige Gespräch knapp aber vollständig zusammen: Ziele, Entscheidungen, \
             wichtige Dateiänderungen und offene Punkte. Gib nur die Zusammenfassung aus.",
        );
        let mut msgs = self.state.messages.clone();
        msgs.push(Message::user_text(instr));

        let summary = {
            let req = CompletionRequest {
                model: &self.state.model,
                system: Some("Du fasst Entwickler-Gespräche präzise und vollständig zusammen."),
                messages: &msgs,
                tools: &[],
                thinking: ThinkingLevel::Off,
                max_tokens: 1024,
            };
            let mut stream = self.provider.stream(req, CancellationToken::new()).await?;
            let mut summary = String::new();
            while let Some(ev) = stream.next().await {
                match ev {
                    StreamEvent::TextDelta { text } => summary.push_str(&text),
                    StreamEvent::Error { message } => return Err(SeppError::Provider(message)),
                    _ => {}
                }
            }
            summary.trim().to_string()
        };
        if summary.is_empty() {
            return Ok(());
        }

        if let Some(store) = self.session.as_mut() {
            // `replaced_until` muss der AKTIVE Leaf sein (nicht der zuletzt angehängte Eintrag):
            // nach einem Branch liegt entries().last() evtl. auf einem verlassenen Ast, und
            // path_messages() würde die Zusammenfassung dann still verwerfen.
            if let Some(leaf) = store.leaf().cloned() {
                store.append(EntryPayload::Compaction {
                    summary: summary.clone(),
                    replaced_until: leaf,
                })?;
            }
        }
        self.state.messages = vec![summary_message(&summary)];
        // Budget nach der Verdichtung neu eichen (sonst bleibt should_compact() wegen des
        // alten last_usage dauerhaft true) und den Compaction-Eintrag dauerhaft sichern.
        self.last_usage = None;
        self.flush_session().await?;
        Ok(())
    }

    /// Treibt einen kompletten Nutzer-Turn bis zum Ende (inkl. aller Tool-Runden).
    pub async fn prompt(
        &mut self,
        text: &str,
        on_event: &(dyn Fn(AgentEvent) + Send + Sync),
        cancel: CancellationToken,
    ) -> Result<()> {
        // before_agent_start: einmalig, kann den System-Prompt ergänzen/ändern.
        if !self.hooks_started {
            self.hooks_started = true;
            if self.hooks.is_some() {
                let mut sp = std::mem::take(&mut self.state.system_prompt);
                if let Some(h) = self.hooks.as_deref() {
                    h.dispatch(HookEvent::BeforeAgentStart {
                        system_prompt: &mut sp,
                    })?;
                }
                self.state.system_prompt = sp;
            }
        }

        // input-Hook: Eingabe transformieren oder direkt behandeln (ohne LLM).
        let mut input_text = text.to_string();
        if let Some(h) = self.hooks.as_deref() {
            if let HookOutcome::Handled = h.dispatch(HookEvent::Input {
                text: &mut input_text,
            })? {
                let user_msg = Message::user_text(input_text);
                self.record(EntryPayload::Message {
                    message: user_msg.clone(),
                })?;
                self.state.messages.push(user_msg);
                self.flush_session().await?;
                on_event(AgentEvent::Done);
                return Ok(());
            }
        }

        // Auto-Compaction VOR dem neuen Prompt (sonst würde er mitsummiert).
        if self.should_compact() {
            self.compact(None).await?;
        }
        let user_msg = Message::user_text(input_text);
        self.record(EntryPayload::Message {
            message: user_msg.clone(),
        })?;
        self.state.messages.push(user_msg);

        let tool_specs: Vec<sepp_core::ToolSpec> =
            self.state.tools.iter().map(|t| t.spec()).collect();

        for _ in 0..self.max_turns {
            if cancel.is_cancelled() {
                return Err(SeppError::Aborted);
            }
            on_event(AgentEvent::TurnStart);

            let req = CompletionRequest {
                model: &self.state.model,
                system: if self.state.system_prompt.is_empty() {
                    None
                } else {
                    Some(self.state.system_prompt.as_str())
                },
                messages: &self.state.messages,
                tools: &tool_specs,
                thinking: self.state.thinking,
                max_tokens: self.max_tokens,
            };

            let mut stream = self.provider.stream(req, cancel.clone()).await?;

            let mut text_buf = String::new();
            let mut thinking_buf = String::new();
            let mut calls: Vec<PendingCall> = Vec::new();
            let mut index_by_id: HashMap<String, usize> = HashMap::new();
            let mut usage: Option<Usage> = None;
            let mut stream_error: Option<String> = None;

            while let Some(ev) = stream.next().await {
                match ev {
                    StreamEvent::MessageStart => {}
                    StreamEvent::TextDelta { text } => {
                        on_event(AgentEvent::TextDelta(text.clone()));
                        text_buf.push_str(&text);
                    }
                    StreamEvent::ThinkingDelta { text } => {
                        on_event(AgentEvent::ThinkingDelta(text.clone()));
                        thinking_buf.push_str(&text);
                    }
                    StreamEvent::ToolUseStart { id, name } => {
                        on_event(AgentEvent::ToolStart {
                            id: id.clone(),
                            name: name.clone(),
                        });
                        index_by_id.insert(id.clone(), calls.len());
                        calls.push(PendingCall {
                            id,
                            name,
                            input_json: String::new(),
                        });
                    }
                    StreamEvent::ToolUseInputDelta { id, partial_json } => {
                        if let Some(&i) = index_by_id.get(&id) {
                            calls[i].input_json.push_str(&partial_json);
                        }
                    }
                    StreamEvent::ToolUseStop { .. } => {}
                    StreamEvent::Usage(u) => usage = Some(u),
                    StreamEvent::MessageStop { .. } => {}
                    StreamEvent::Error { message } => {
                        stream_error = Some(message);
                        break;
                    }
                }
            }
            drop(stream); // löst die immutable Borrows auf self, bevor wir mutieren

            if let Some(message) = stream_error {
                on_event(AgentEvent::Error(message.clone()));
                // Bis hierher aufgezeichnete Einträge (mind. die User-Message) durabel machen,
                // ohne den Provider-Fehler zu maskieren — Audit-Trail auch bei Fehlern.
                let _ = self.flush_session().await;
                return Err(SeppError::Provider(message));
            }
            if cancel.is_cancelled() {
                return Err(SeppError::Aborted);
            }

            // Assistant-Nachricht rekonstruieren (Thinking?, Text?, ToolUse*).
            let mut content: Vec<ContentBlock> = Vec::new();
            if !thinking_buf.is_empty() {
                content.push(ContentBlock::Thinking {
                    text: thinking_buf,
                    signature: None,
                });
            }
            if !text_buf.is_empty() {
                content.push(ContentBlock::text(text_buf));
            }
            for call in &calls {
                content.push(ContentBlock::ToolUse {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: parse_input(&call.input_json).unwrap_or_else(|_| json!({})),
                });
            }
            let mut assistant = Message::assistant(content);
            assistant.usage = usage;
            if let Some(u) = usage {
                // Letzter Provider-Stream eicht das Kontext-Budget (Auto-Compaction-Schwelle).
                self.last_usage = Some(u);
            }
            if let Some(h) = self.hooks.as_deref() {
                let _ = h.dispatch(HookEvent::TurnEnd {
                    message: &assistant,
                });
            }
            self.record(EntryPayload::Message {
                message: assistant.clone(),
            })?;
            self.state.messages.push(assistant);
            on_event(AgentEvent::TurnEnd);

            if calls.is_empty() {
                self.flush_session().await?;
                on_event(AgentEvent::Done);
                return Ok(());
            }

            // Tool-Calls parallel ausführen, Reihenfolge erhalten.
            let results = self.run_tools(&calls, &cancel, on_event).await?;

            let mut tr_content: Vec<ContentBlock> = Vec::with_capacity(results.len());
            for (call, res) in calls.iter().zip(results) {
                let (blocks, is_error) = match res {
                    Ok(r) => (r.content, r.is_error),
                    Err(SeppError::Aborted) => return Err(SeppError::Aborted),
                    Err(e) => (vec![ContentBlock::text(e.to_string())], true),
                };
                tr_content.push(ContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content: blocks,
                    is_error,
                });
            }
            let tool_msg = Message {
                role: Role::User,
                content: tr_content,
                usage: None,
            };
            self.record(EntryPayload::Message {
                message: tool_msg.clone(),
            })?;
            self.state.messages.push(tool_msg);
        }

        Err(SeppError::Provider(format!(
            "max_turns ({}) erreicht",
            self.max_turns
        )))
    }

    async fn run_tools(
        &self,
        calls: &[PendingCall],
        cancel: &CancellationToken,
        on_event: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<Vec<Result<sepp_core::ToolResult>>> {
        let mut set: JoinSet<(usize, Result<sepp_core::ToolResult>)> = JoinSet::new();
        let mut slots: Vec<Option<Result<sepp_core::ToolResult>>> =
            (0..calls.len()).map(|_| None).collect();

        for (i, call) in calls.iter().enumerate() {
            // Input parsen; Parse-Fehler werden zum (Fehler-)Ergebnis.
            let mut input = match parse_input(&call.input_json) {
                Ok(v) => v,
                Err(e) => {
                    on_event(AgentEvent::ToolEnd {
                        id: call.id.clone(),
                        is_error: true,
                    });
                    slots[i] = Some(Err(e));
                    continue;
                }
            };

            // tool_call-Hook: blocken oder durchlassen (sequenziell, vor dem Spawn).
            // Hinweis: Der `RhaiHookHost` blockt nur, er verändert `input` nicht. Sobald Hooks
            // Argumente patchen dürfen (Phase 4), muss der Hook VOR dem Bau der Assistant-Message
            // laufen — sonst protokolliert die Session den rohen statt des gepatchten Inputs.
            if let Some(h) = self.hooks.as_deref() {
                match h.dispatch(HookEvent::ToolCall {
                    name: &call.name,
                    input: &mut input,
                }) {
                    Ok(HookOutcome::Block { reason }) => {
                        on_event(AgentEvent::ToolEnd {
                            id: call.id.clone(),
                            is_error: true,
                        });
                        slots[i] = Some(Ok(sepp_core::ToolResult {
                            content: vec![ContentBlock::text(reason)],
                            details: json!({ "blocked": true }),
                            is_error: true,
                        }));
                        continue;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        on_event(AgentEvent::ToolEnd {
                            id: call.id.clone(),
                            is_error: true,
                        });
                        slots[i] = Some(Err(e));
                        continue;
                    }
                }
            }

            let tool = self.tools_by_name.get(&call.name).cloned();
            let name = call.name.clone();
            let cancel = cancel.clone();
            set.spawn(async move {
                let r = match tool {
                    None => Err(SeppError::Tool(format!("unbekanntes Tool: {name}"))),
                    Some(tool) => tool.execute(input, cancel, None).await,
                };
                (i, r)
            });
        }

        while let Some(joined) = set.join_next().await {
            let (i, mut r) =
                joined.map_err(|e| SeppError::Tool(format!("tool task fehlgeschlagen: {e}")))?;
            // tool_result-Hook (beobachtend).
            if let (Some(h), Ok(tr)) = (self.hooks.as_deref(), r.as_mut()) {
                let _ = h.dispatch(HookEvent::ToolResult {
                    name: &calls[i].name,
                    result: tr,
                });
            }
            let is_error = match &r {
                Ok(tr) => tr.is_error,
                Err(_) => true,
            };
            on_event(AgentEvent::ToolEnd {
                id: calls[i].id.clone(),
                is_error,
            });
            slots[i] = Some(r);
        }

        Ok(slots
            .into_iter()
            .map(|o| o.unwrap_or_else(|| Err(SeppError::Tool("kein Tool-Ergebnis".into()))))
            .collect())
    }
}

fn parse_input(s: &str) -> Result<Value> {
    if s.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(s).map_err(|e| SeppError::Tool(format!("ungültiges Tool-JSON: {e}")))
}

/// Grobe Token-Schätzung (≈ 4 Zeichen/Token) über alle Content-Blöcke.
fn estimate_tokens(messages: &[Message]) -> u64 {
    let mut chars = 0usize;
    for m in messages {
        for b in &m.content {
            match b {
                ContentBlock::Text { text } => chars += text.len(),
                ContentBlock::Thinking { text, .. } => chars += text.len(),
                ContentBlock::ToolUse { input, .. } => chars += input.to_string().len(),
                ContentBlock::ToolResult { content, .. } => {
                    for c in content {
                        if let ContentBlock::Text { text } = c {
                            chars += text.len();
                        }
                    }
                }
                ContentBlock::Image { .. } => chars += 1024,
            }
        }
        chars += 8; // grober Rollen-/Struktur-Overhead
    }
    (chars / 4) as u64
}

/// Baut eine [`AgentSession`] aus ihren Teilen.
#[derive(Default)]
pub struct AgentSessionBuilder {
    provider: Option<Arc<dyn Provider>>,
    model: Option<Model>,
    system_prompt: String,
    thinking: ThinkingLevel,
    tools: Vec<Arc<dyn Tool>>,
    max_tokens: u64,
    max_turns: usize,
    session: Option<Box<dyn SessionStore>>,
    auto_compact_threshold: Option<u64>,
    hooks: Option<Box<dyn HookHost>>,
}

impl AgentSessionBuilder {
    pub fn provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }
    pub fn model(mut self, model: Model) -> Self {
        self.model = Some(model);
        self
    }
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }
    pub fn thinking(mut self, level: ThinkingLevel) -> Self {
        self.thinking = level;
        self
    }
    pub fn tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.tools = tools;
        self
    }
    pub fn max_tokens(mut self, n: u64) -> Self {
        self.max_tokens = n;
        self
    }
    pub fn max_turns(mut self, n: usize) -> Self {
        self.max_turns = n;
        self
    }
    /// Persistenz-Backend; wird beim Build genutzt, um die Conversation zu seeden
    /// (Fortsetzen) und neue Schritte zu protokollieren.
    pub fn session(mut self, store: Box<dyn SessionStore>) -> Self {
        self.session = Some(store);
        self
    }
    /// Schwelle (geschätzte Tokens), ab der vor einem neuen Prompt auto-komprimiert wird.
    pub fn auto_compact_threshold(mut self, tokens: u64) -> Self {
        self.auto_compact_threshold = Some(tokens);
        self
    }
    /// Hook-Host (Tier 1) für Eingriffe in den Loop.
    pub fn hooks(mut self, hooks: Box<dyn HookHost>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Baut die Session (Default-Werte: thinking=off, max_tokens=4096, max_turns=50).
    pub fn build(self) -> Result<AgentSession> {
        let provider = self
            .provider
            .ok_or_else(|| SeppError::Config("AgentSession: provider fehlt".into()))?;
        let model = self
            .model
            .ok_or_else(|| SeppError::Config("AgentSession: model fehlt".into()))?;

        let max_tokens = if self.max_tokens == 0 {
            4096
        } else {
            self.max_tokens
        };
        let max_turns = if self.max_turns == 0 {
            50
        } else {
            self.max_turns
        };

        let tools_by_name = self
            .tools
            .iter()
            .map(|t| (t.spec().name, Arc::clone(t)))
            .collect();

        // Beim Fortsetzen die aktive Conversation aus dem Store seeden (ohne sie erneut
        // zu protokollieren — sie steht ja schon in der Datei).
        let messages = match &self.session {
            Some(s) => s.path_messages(),
            None => Vec::new(),
        };

        Ok(AgentSession {
            provider,
            state: AgentState {
                messages,
                model,
                system_prompt: self.system_prompt,
                thinking: self.thinking,
                tools: self.tools,
            },
            tools_by_name,
            max_tokens,
            max_turns,
            session: self.session,
            auto_compact_threshold: self.auto_compact_threshold,
            last_usage: None,
            hooks: self.hooks,
            hooks_started: false,
        })
    }
}
