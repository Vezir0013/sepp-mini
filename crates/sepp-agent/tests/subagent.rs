//! Native Sub-Agenten: eine Teilaufgabe läuft isoliert; nur das Endergebnis kehrt zurück.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

use futures::stream::{self, BoxStream};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use sepp_agent::SubAgentTool;
use sepp_core::{ContentBlock, Model, Result, ToolResult, ToolSpec, Usage};
use sepp_provider::{CompletionRequest, Provider, StopReason, StreamEvent};
use sepp_tools::Tool;

struct FakeProvider {
    scripts: Mutex<VecDeque<Vec<StreamEvent>>>,
}

#[async_trait::async_trait]
impl Provider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }
    async fn stream<'a>(
        &'a self,
        _req: CompletionRequest<'a>,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'a, StreamEvent>> {
        let events = self
            .scripts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or_default();
        Ok(Box::pin(stream::iter(events)))
    }
}

struct StaticTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for StaticTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "inner".into(),
            label: "Inner".into(),
            description: "Sub-Agent-internes Tool".into(),
            parameters: json!({ "type": "object" }),
        }
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _cancel: CancellationToken,
        _on_update: Option<&(dyn Fn(ToolResult) + Send + Sync)>,
    ) -> Result<ToolResult> {
        self.calls.fetch_add(1, SeqCst);
        Ok(ToolResult::text("inneres-ergebnis"))
    }
}

fn model() -> Model {
    Model {
        id: "fake-1".into(),
        provider: "fake".into(),
        display_name: "Fake".into(),
        context_window: 100_000,
        max_output_tokens: 4096,
        supports_reasoning: false,
        supports_images: false,
    }
}

#[tokio::test]
async fn subagent_returns_only_final_answer() {
    let script = vec![
        StreamEvent::MessageStart,
        StreamEvent::TextDelta {
            text: "Antwort: 42".into(),
        },
        StreamEvent::MessageStop {
            stop_reason: StopReason::EndTurn,
        },
    ];
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::from(vec![script])),
    });
    let tool = SubAgentTool::new(provider, model());

    let res = tool
        .execute(
            json!({ "description": "Was ist 6*7?" }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    assert!(!res.is_error);
    assert!(matches!(&res.content[0], ContentBlock::Text { text } if text == "Antwort: 42"));
}

/// Beweist die Isolation: der Sub-Agent durchläuft intern eine Tool-Use-Runde, aber an die Wurzel
/// kehrt **nur** die finale Antwort zurück — die interne Runde bläht den Wurzel-Kontext nicht auf.
#[tokio::test]
async fn subagent_isolates_internal_turns() {
    let calls = Arc::new(AtomicUsize::new(0));
    let inner: Arc<dyn Tool> = Arc::new(StaticTool {
        calls: calls.clone(),
    });

    let turn1 = vec![
        StreamEvent::MessageStart,
        StreamEvent::ToolUseStart {
            id: "a".into(),
            name: "inner".into(),
        },
        StreamEvent::ToolUseInputDelta {
            id: "a".into(),
            partial_json: "{}".into(),
        },
        StreamEvent::ToolUseStop { id: "a".into() },
        StreamEvent::Usage(Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        }),
        StreamEvent::MessageStop {
            stop_reason: StopReason::ToolUse,
        },
    ];
    let turn2 = vec![
        StreamEvent::MessageStart,
        StreamEvent::TextDelta {
            text: "fertig: ok".into(),
        },
        StreamEvent::MessageStop {
            stop_reason: StopReason::EndTurn,
        },
    ];
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::from(vec![turn1, turn2])),
    });
    let tool = SubAgentTool::new(provider, model()).tools(vec![inner]);

    let res = tool
        .execute(
            json!({ "description": "nutze das innere Tool" }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        calls.load(SeqCst),
        1,
        "Sub-Agent hat das interne Tool ausgeführt"
    );
    // Wurzel sieht NUR die finale Antwort, nicht die interne Tool-Use-Runde.
    assert!(matches!(&res.content[0], ContentBlock::Text { text } if text == "fertig: ok"));
}

#[tokio::test]
async fn subagent_empty_answer_is_surfaced_not_silent() {
    // Turn ohne Text (z. B. max_turns) → kein stummes leeres Ergebnis.
    let script = vec![
        StreamEvent::MessageStart,
        StreamEvent::MessageStop {
            stop_reason: StopReason::EndTurn,
        },
    ];
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::from(vec![script])),
    });
    let tool = SubAgentTool::new(provider, model());
    let res = tool
        .execute(
            json!({ "description": "x" }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(&res.content[0],
        ContentBlock::Text { text } if text.contains("keine Textantwort")));
}

#[tokio::test]
async fn subagent_missing_description_errors() {
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::new()),
    });
    let tool = SubAgentTool::new(provider, model());
    let res = tool
        .execute(json!({}), CancellationToken::new(), None)
        .await;
    assert!(res.is_err());
}
