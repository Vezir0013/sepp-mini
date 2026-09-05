//! Native Sub-Agenten (Phase 4): eine Teilaufgabe läuft in einer **isolierten** [`AgentSession`]
//! (eigene Conversation, eingeschränktes Toolset, eigenes Budget). Nur das Endergebnis kehrt als
//! [`ToolResult`] zur Wurzel zurück — der Wurzel-Kontext bleibt schlank.
//!
//! Für das Audit ist Isolation zu wenig: ohne Spur wäre alles, was der Sub-Agent tut, unsichtbar.
//! Mit einer [`SessionFactory`] schreibt jeder Lauf deshalb eine **eigene Kind-Session**, die im
//! Header auf die Wurzel verweist, und meldet sie der Wurzel über den reservierten Schlüssel
//! `details["audit"]` (siehe [`crate::AUDIT_DETAIL_KEY`]).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use sepp_core::{
    ContentBlock, Message, Model, Result, Role, SeppError, ThinkingLevel, ToolResult, ToolSpec,
};
use sepp_provider::Provider;
use sepp_session::SessionStore;
use sepp_tools::Tool;

use crate::{AgentEvent, AgentSession, AUDIT_DETAIL_KEY};

/// Erzeugt den Session-Store für einen Sub-Agent-Lauf. Das Frontend entscheidet Backend,
/// Verzeichnis und Wurzel-Verweis; liefert `None`, wenn nicht persistiert werden soll.
///
/// `Arc` + `Send + Sync`, weil [`Tool::execute`] nur `&self` bekommt und mehrere `task`-Aufrufe
/// im selben Turn nebenläufig laufen können.
pub type SessionFactory = Arc<dyn Fn() -> Option<Box<dyn SessionStore>> + Send + Sync>;

/// Wie viele Zeichen der Aufgabenstellung in den Audit-Eintrag wandern.
const TASK_PREVIEW_CHARS: usize = 160;

/// Ein Tool, das eine Teilaufgabe an einen frisch aufgesetzten Sub-Agenten delegiert.
pub struct SubAgentTool {
    provider: Arc<dyn Provider>,
    model: Model,
    tools: Vec<Arc<dyn Tool>>,
    system_prompt: String,
    max_tokens: u64,
    max_turns: usize,
    thinking: ThinkingLevel,
    name: String,
    description: String,
    session_factory: Option<SessionFactory>,
}

impl SubAgentTool {
    /// Neuer Sub-Agent mit Provider + Modell (Default: Tool-Name `task`, leeres Toolset).
    pub fn new(provider: Arc<dyn Provider>, model: Model) -> Self {
        SubAgentTool {
            provider,
            model,
            tools: Vec::new(),
            system_prompt: "Du bist ein fokussierter Sub-Agent. Löse die Teilaufgabe \
                            eigenständig und antworte am Ende knapp mit dem Ergebnis."
                .into(),
            max_tokens: 4096,
            max_turns: 20,
            thinking: ThinkingLevel::Off,
            name: "task".into(),
            description: "Delegiert eine in sich geschlossene Teilaufgabe an einen isolierten \
                          Sub-Agenten (eigener Kontext, eingeschränktes Toolset, eigenes Budget). \
                          Gibt nur das Endergebnis zurück."
                .into(),
            session_factory: None,
        }
    }

    /// Eingeschränktes Toolset des Sub-Agenten (Default: leer).
    pub fn tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.tools = tools;
        self
    }
    /// System-Prompt des Sub-Agenten.
    pub fn system_prompt(mut self, p: impl Into<String>) -> Self {
        self.system_prompt = p.into();
        self
    }
    /// Max. Output-Tokens je Sub-Agent-Turn.
    pub fn max_tokens(mut self, n: u64) -> Self {
        self.max_tokens = n;
        self
    }
    /// Max. Anzahl Turns im Sub-Agent-Loop.
    pub fn max_turns(mut self, n: usize) -> Self {
        self.max_turns = n;
        self
    }
    /// Reasoning-Stufe des Sub-Agenten (Default: Off; die Haupt-Session reicht ihre Stufe durch,
    /// damit z. B. eine z.ai-Session durchgängig reasoning-AN läuft).
    pub fn thinking(mut self, level: ThinkingLevel) -> Self {
        self.thinking = level;
        self
    }
    /// Exponierter Tool-Name (für Kollisions-Präfixe).
    pub fn name(mut self, n: impl Into<String>) -> Self {
        self.name = n.into();
        self
    }
    /// Store-Fabrik für die Kind-Session je Lauf (siehe [`SessionFactory`]). Ohne sie bleibt
    /// der Sub-Agent wie bisher flüchtig.
    pub fn session_factory(mut self, f: SessionFactory) -> Self {
        self.session_factory = Some(f);
        self
    }
}

/// Einzeilige, gekürzte Vorschau der Aufgabenstellung für den Audit-Eintrag.
fn task_preview(task: &str) -> String {
    let one: String = task.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= TASK_PREVIEW_CHARS {
        return one;
    }
    let head: String = one.chars().take(TASK_PREVIEW_CHARS).collect();
    format!("{head}…")
}

fn last_assistant_text(messages: &[Message]) -> String {
    for m in messages.iter().rev() {
        if m.role == Role::Assistant {
            let mut s = String::new();
            for b in &m.content {
                if let ContentBlock::Text { text } = b {
                    if !s.is_empty() {
                        s.push('\n');
                    }
                    s.push_str(text);
                }
            }
            if !s.is_empty() {
                return s;
            }
        }
    }
    String::new()
}

#[async_trait]
impl Tool for SubAgentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            label: "Sub-Agent".into(),
            description: self.description.clone(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "Die Teilaufgabe, die der Sub-Agent lösen soll."
                    }
                },
                "required": ["description"]
            }),
        }
    }

    async fn execute(
        &self,
        input: Value,
        cancel: CancellationToken,
        _on_update: Option<&(dyn Fn(ToolResult) + Send + Sync)>,
    ) -> Result<ToolResult> {
        let task = input
            .get("description")
            .or_else(|| input.get("prompt"))
            .or_else(|| input.get("task"))
            .and_then(Value::as_str)
            .ok_or_else(|| SeppError::Tool("sub-agent: Feld 'description' fehlt".into()))?;

        // Frische, isolierte Conversation und eigenes Budget; der Store — falls es einen gibt —
        // ist eine eigene Kind-Datei, nicht die der Wurzel.
        let store = self.session_factory.as_ref().and_then(|f| f());
        let child_id = store.as_ref().map(|s| s.id().to_string());
        let mut builder = AgentSession::builder()
            .provider(Arc::clone(&self.provider))
            .model(self.model.clone())
            .system_prompt(self.system_prompt.clone())
            .tools(self.tools.clone())
            .max_tokens(self.max_tokens)
            .max_turns(self.max_turns)
            .thinking(self.thinking);
        if let Some(store) = store {
            builder = builder.session(store);
        }
        let mut sub = builder.build()?;

        // Sub-Agent-Ereignisse werden bewusst NICHT an die Wurzel weitergereicht.
        let sink = |_ev: AgentEvent| {};
        let outcome = sub.prompt(task, &sink, cancel).await;

        // Auch ein abgebrochener Lauf muss auf Platte stehen — sonst fehlt genau der Fall
        // in der Spur, den man hinterher nachlesen will. `JsonlSessionStore` hat kein `Drop`,
        // ohne `finalize` bliebe der Puffer ungeschrieben. Ein Schreibfehler der Spur darf
        // dabei nie den Turn abbrechen, deshalb `let _`.
        let entries = sub.session().map(|s| s.entries().len()).unwrap_or(0);
        let _ = sub.finalize().await;

        // Das Audit-Objekt entsteht VOR der Fehlerbehandlung: Sonst verlöre ausgerechnet der
        // gescheiterte Lauf seinen Verweis, und die Kind-Session läge verwaist auf Platte,
        // ohne dass in der Wurzel irgendetwas auf sie zeigt.
        let audit = child_id.map(|id| {
            json!({
                AUDIT_DETAIL_KEY_KIND: "subagent",
                "tool": self.name,
                "session": id,
                "task": task_preview(task),
                "entries": entries,
            })
        });

        if let Err(e) = outcome {
            // Ctrl+C reicht durch — der ganze Lauf wird gerade abgeräumt, und ein Abbruch ist
            // kein Werkzeugfehler, auf den das Modell reagieren sollte.
            if matches!(e, SeppError::Aborted) {
                return Err(e);
            }
            // Alles andere ist ein gescheiterter Auftrag: Das Modell soll es sehen und darauf
            // reagieren können, und die Spur behält ihren Verweis auf die Kind-Session.
            let mut result = ToolResult::text(format!("Sub-Agent fehlgeschlagen: {e}"));
            result.is_error = true;
            return Ok(with_audit(result, audit));
        }

        let answer = last_assistant_text(sub.messages());
        if answer.is_empty() {
            // Kein Text-Ergebnis (z. B. max_turns erreicht) — nicht stumm als Erfolg ausgeben.
            return Ok(with_audit(
                ToolResult::text("(Sub-Agent lieferte keine Textantwort)"),
                audit,
            ));
        }
        // Wie jedes Tool-Output gekürzt, bevor es zurück in die Wurzel-Conversation fließt.
        let t = sepp_tools::truncate_head(
            &answer,
            sepp_tools::DEFAULT_MAX_LINES,
            sepp_tools::DEFAULT_MAX_BYTES,
        );
        let note = t.note();
        let mut text = t.content;
        if let Some(note) = note {
            text.push_str(&note);
        }
        Ok(with_audit(ToolResult::text(text), audit))
    }
}

/// Feldname der Eintragsart innerhalb des Audit-Objekts.
const AUDIT_DETAIL_KEY_KIND: &str = "kind";

/// Hängt den Verweis auf die Kind-Session an `details["audit"]` (Details gehen nicht ans Modell).
fn with_audit(result: ToolResult, audit: Option<Value>) -> ToolResult {
    match audit {
        Some(a) => result.with_details(json!({ AUDIT_DETAIL_KEY: a })),
        None => result,
    }
}
