//! Native Sub-Agenten (Phase 4): eine Teilaufgabe läuft in einer **isolierten** [`AgentSession`]
//! (eigene Conversation, eingeschränktes Toolset, eigenes Budget). Nur das Endergebnis kehrt als
//! [`ToolResult`] zur Wurzel zurück — der Wurzel-Kontext bleibt schlank (`docs/07`-Akzeptanz).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use sepp_core::{
    ContentBlock, Message, Model, Result, Role, SeppError, ThinkingLevel, ToolResult, ToolSpec,
};
use sepp_provider::Provider;
use sepp_tools::Tool;

use crate::{AgentEvent, AgentSession};

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

        // Frische, isolierte Session (kein SessionStore → eigene leere Conversation, eigenes Budget).
        let mut sub = AgentSession::builder()
            .provider(Arc::clone(&self.provider))
            .model(self.model.clone())
            .system_prompt(self.system_prompt.clone())
            .tools(self.tools.clone())
            .max_tokens(self.max_tokens)
            .max_turns(self.max_turns)
            .thinking(self.thinking)
            .build()?;

        // Sub-Agent-Ereignisse werden bewusst NICHT an die Wurzel weitergereicht.
        let sink = |_ev: AgentEvent| {};
        sub.prompt(task, &sink, cancel).await?;

        let answer = last_assistant_text(sub.messages());
        if answer.is_empty() {
            // Kein Text-Ergebnis (z. B. max_turns erreicht) — nicht stumm als Erfolg ausgeben.
            return Ok(ToolResult::text("(Sub-Agent lieferte keine Textantwort)"));
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
        Ok(ToolResult::text(text))
    }
}
