//! Deterministischer Agent-Loop-Test: `FakeProvider` (gescriptete StreamEvents) + ein
//! In-Memory-Tool, kein Netz, kein API-Key.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

use futures::stream::{self, BoxStream};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use sepp_agent::{AgentEvent, AgentSession};
use sepp_core::{Model, Result, Role, ToolResult, ToolSpec, Usage};
use sepp_provider::{CompletionRequest, Provider, StopReason, StreamEvent};
use sepp_session::SessionStore; // Trait im Scope für `.entries()` auf konkretem JsonlSessionStore
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
    name: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for StaticTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            label: "Tool".into(),
            description: "Test-Tool".into(),
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
        Ok(ToolResult::text("ERGEBNIS"))
    }
}

fn test_model() -> Model {
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
async fn runs_tool_then_finishes() {
    let calls = Arc::new(AtomicUsize::new(0));
    let tool: Arc<dyn Tool> = Arc::new(StaticTool {
        name: "echo".into(),
        calls: calls.clone(),
    });

    let script1 = vec![
        StreamEvent::MessageStart,
        StreamEvent::TextDelta {
            text: "Arbeite…".into(),
        },
        StreamEvent::ToolUseStart {
            id: "t1".into(),
            name: "echo".into(),
        },
        StreamEvent::ToolUseInputDelta {
            id: "t1".into(),
            partial_json: "{\"x\":1}".into(),
        },
        StreamEvent::ToolUseStop { id: "t1".into() },
        StreamEvent::Usage(Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        }),
        StreamEvent::MessageStop {
            stop_reason: StopReason::ToolUse,
        },
    ];
    let script2 = vec![
        StreamEvent::MessageStart,
        StreamEvent::TextDelta {
            text: "Fertig.".into(),
        },
        StreamEvent::MessageStop {
            stop_reason: StopReason::EndTurn,
        },
    ];

    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::from(vec![script1, script2])),
    });

    let mut session = AgentSession::builder()
        .provider(provider)
        .model(test_model())
        .tools(vec![tool])
        .build()
        .unwrap();

    let text = Arc::new(Mutex::new(String::new()));
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let text2 = text.clone();
    let log2 = log.clone();
    let on_event = move |ev: AgentEvent| {
        if let AgentEvent::TextDelta(t) = &ev {
            text2.lock().unwrap().push_str(t);
        }
        log2.lock().unwrap().push(format!("{ev:?}"));
    };

    session
        .prompt("hallo", &on_event, CancellationToken::new())
        .await
        .unwrap();

    // Tool wurde genau einmal aufgerufen.
    assert_eq!(calls.load(SeqCst), 1);

    // Gestreamter Text beider Turns kam an.
    let text = text.lock().unwrap().clone();
    assert!(text.contains("Arbeite"));
    assert!(text.contains("Fertig"));

    // user, assistant(tool_use), user(tool_result), assistant(final).
    assert_eq!(session.state().messages.len(), 4);

    let log = log.lock().unwrap();
    assert!(log.iter().any(|s| s.contains("ToolStart")));
    assert!(log.iter().any(|s| s.contains("ToolEnd")));
    assert!(log.iter().any(|s| s.contains("Done")));
}

#[tokio::test]
async fn unknown_tool_yields_error_result_not_panic() {
    let script1 = vec![
        StreamEvent::MessageStart,
        StreamEvent::ToolUseStart {
            id: "t1".into(),
            name: "does_not_exist".into(),
        },
        StreamEvent::ToolUseStop { id: "t1".into() },
        StreamEvent::MessageStop {
            stop_reason: StopReason::ToolUse,
        },
    ];
    let script2 = vec![
        StreamEvent::MessageStart,
        StreamEvent::TextDelta { text: "ok".into() },
        StreamEvent::MessageStop {
            stop_reason: StopReason::EndTurn,
        },
    ];
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::from(vec![script1, script2])),
    });

    let mut session = AgentSession::builder()
        .provider(provider)
        .model(test_model())
        .tools(vec![])
        .build()
        .unwrap();

    let noop = |_ev: AgentEvent| {};
    session
        .prompt("x", &noop, CancellationToken::new())
        .await
        .unwrap();

    // Die Tool-Result-Message muss als Fehler markiert sein.
    let msgs = &session.state().messages;
    let tool_result_is_error = msgs.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, sepp_core::ContentBlock::ToolResult { is_error, .. } if *is_error))
    });
    assert!(tool_result_is_error);
}

fn text_turn(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::MessageStart,
        StreamEvent::TextDelta { text: text.into() },
        StreamEvent::MessageStop {
            stop_reason: StopReason::EndTurn,
        },
    ]
}

fn text_turn_usage(text: &str, usage: Usage) -> Vec<StreamEvent> {
    vec![
        StreamEvent::MessageStart,
        StreamEvent::TextDelta { text: text.into() },
        StreamEvent::Usage(usage),
        StreamEvent::MessageStop {
            stop_reason: StopReason::EndTurn,
        },
    ]
}

#[tokio::test]
async fn accumulates_total_usage_across_turns() {
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::from(vec![
            text_turn_usage(
                "a",
                Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    ..Default::default()
                },
            ),
            text_turn_usage(
                "b",
                Usage {
                    input_tokens: 50,
                    output_tokens: 10,
                    cache_read_tokens: 7,
                    ..Default::default()
                },
            ),
        ])),
    });
    let mut session = AgentSession::builder()
        .provider(provider)
        .model(test_model())
        .tools(vec![])
        .build()
        .unwrap();

    let noop = |_e: AgentEvent| {};
    session
        .prompt("1", &noop, CancellationToken::new())
        .await
        .unwrap();
    session
        .prompt("2", &noop, CancellationToken::new())
        .await
        .unwrap();

    let u = session.total_usage();
    assert_eq!(u.input_tokens, 150);
    assert_eq!(u.output_tokens, 30);
    assert_eq!(u.cache_read_tokens, 7);
    assert_eq!(session.usage_turns(), 2);
}

#[tokio::test]
async fn total_usage_survives_compaction() {
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::from(vec![
            text_turn_usage(
                "a",
                Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    ..Default::default()
                },
            ),
            text_turn("SUMMARY"), // compact() ruft den Provider — ohne Usage, zählt nicht mit
            text_turn_usage(
                "b",
                Usage {
                    input_tokens: 50,
                    output_tokens: 10,
                    ..Default::default()
                },
            ),
        ])),
    });
    let store = Box::new(sepp_session::InMemorySessionStore::new());
    let mut session = AgentSession::builder()
        .provider(provider)
        .model(test_model())
        .tools(vec![])
        .session(store)
        .build()
        .unwrap();

    let noop = |_e: AgentEvent| {};
    session
        .prompt("erste", &noop, CancellationToken::new())
        .await
        .unwrap();
    session.compact(None).await.unwrap();
    session
        .prompt("zweite", &noop, CancellationToken::new())
        .await
        .unwrap();

    // Summe übersteht die Compaction (anders als last_usage, das genullt wird).
    let u = session.total_usage();
    assert_eq!(u.input_tokens, 150);
    assert_eq!(u.output_tokens, 30);
    assert_eq!(session.usage_turns(), 2);
}

#[tokio::test]
async fn finalize_writes_summary_and_flushes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Box::new(sepp_session::JsonlSessionStore::create(dir.path()).unwrap());
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::from(vec![text_turn_usage(
            "hi",
            Usage {
                input_tokens: 42,
                output_tokens: 7,
                ..Default::default()
            },
        )])),
    });
    let mut session = AgentSession::builder()
        .provider(provider)
        .model(test_model())
        .tools(vec![])
        .session(store)
        .build()
        .unwrap();

    let noop = |_e: AgentEvent| {};
    session
        .prompt("frage", &noop, CancellationToken::new())
        .await
        .unwrap();
    session.finalize().await.unwrap();
    session.finalize().await.unwrap(); // idempotent: schreibt keinen zweiten Eintrag
    drop(session);

    let infos = sepp_session::JsonlSessionStore::list(dir.path()).unwrap();
    assert_eq!(infos.len(), 1);
    let reopened = sepp_session::JsonlSessionStore::open(&infos[0].path).unwrap();
    let summaries: Vec<_> = reopened
        .entries()
        .iter()
        .filter_map(|e| match &e.payload {
            sepp_session::EntryPayload::Custom { kind, data } if kind == "usage_summary" => {
                Some(data.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        summaries.len(),
        1,
        "genau ein usage_summary erwartet (idempotent)"
    );
    assert_eq!(summaries[0]["input_tokens"], 42);
    assert_eq!(summaries[0]["output_tokens"], 7);
    assert_eq!(summaries[0]["turns"], 1);
}

#[tokio::test]
async fn provider_error_flushes_recorded_entries() {
    let dir = tempfile::tempdir().unwrap();
    let store = Box::new(sepp_session::JsonlSessionStore::create(dir.path()).unwrap());
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::from(vec![vec![
            StreamEvent::MessageStart,
            StreamEvent::Error {
                message: "boom".into(),
            },
        ]])),
    });
    let mut session = AgentSession::builder()
        .provider(provider)
        .model(test_model())
        .tools(vec![])
        .session(store)
        .build()
        .unwrap();

    let noop = |_e: AgentEvent| {};
    let res = session
        .prompt("frage", &noop, CancellationToken::new())
        .await;
    assert!(res.is_err());

    // Datei lesen, WÄHREND die Session noch lebt → prüft den fsync im Fehlerpfad (nicht erst beim
    // Drop). Ohne den Fehlerpfad-Flush stünde die User-Message nur im BufWriter, nicht auf Platte.
    let infos = sepp_session::JsonlSessionStore::list(dir.path()).unwrap();
    let reopened = sepp_session::JsonlSessionStore::open(&infos[0].path).unwrap();
    let has_user = reopened.entries().iter().any(|e| {
        matches!(&e.payload, sepp_session::EntryPayload::Message { message } if message.role == Role::User)
    });
    assert!(
        has_user,
        "User-Message sollte trotz Provider-Fehler durabel persistiert sein"
    );
}

#[tokio::test]
async fn persists_user_and_assistant_to_session_store() {
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::from(vec![text_turn("hi")])),
    });
    let store = Box::new(sepp_session::InMemorySessionStore::new());
    let mut session = AgentSession::builder()
        .provider(provider)
        .model(test_model())
        .tools(vec![])
        .session(store)
        .build()
        .unwrap();

    let noop = |_ev: AgentEvent| {};
    session
        .prompt("frage", &noop, CancellationToken::new())
        .await
        .unwrap();

    let store = session.session().unwrap();
    assert_eq!(store.entries().len(), 2); // user + assistant
    assert_eq!(store.path_messages().len(), 2);
}

#[tokio::test]
async fn auto_compacts_when_over_threshold() {
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::from(vec![
            text_turn("Antwort 1"),       // erster Prompt
            text_turn("ZUSAMMENFASSUNG"), // Compaction-Aufruf
            text_turn("Antwort 2"),       // zweiter Prompt
        ])),
    });
    let store = Box::new(sepp_session::InMemorySessionStore::new());
    let mut session = AgentSession::builder()
        .provider(provider)
        .model(test_model())
        .tools(vec![])
        .session(store)
        .auto_compact_threshold(1) // praktisch immer komprimieren
        .build()
        .unwrap();

    let noop = |_ev: AgentEvent| {};
    session
        .prompt("erste", &noop, CancellationToken::new())
        .await
        .unwrap();
    session
        .prompt("zweite", &noop, CancellationToken::new())
        .await
        .unwrap();

    let has_compaction = session
        .session()
        .unwrap()
        .entries()
        .iter()
        .any(|e| matches!(e.payload, sepp_session::EntryPayload::Compaction { .. }));
    assert!(has_compaction);

    assert!(matches!(&session.messages()[0].content[0],
        sepp_core::ContentBlock::Text { text } if text.contains("ZUSAMMENFASSUNG")));
}

#[tokio::test]
async fn tool_call_hook_blocks_execution_end_to_end() {
    // Turn 1: Modell ruft "bash" mit `rm -rf` auf; Turn 2: Abschlusstext.
    let script1 = vec![
        StreamEvent::MessageStart,
        StreamEvent::ToolUseStart {
            id: "t1".into(),
            name: "bash".into(),
        },
        StreamEvent::ToolUseInputDelta {
            id: "t1".into(),
            partial_json: "{\"command\":\"rm -rf /\"}".into(),
        },
        StreamEvent::ToolUseStop { id: "t1".into() },
        StreamEvent::MessageStop {
            stop_reason: StopReason::ToolUse,
        },
    ];
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::from(vec![script1, text_turn("erledigt")])),
    });

    let calls = Arc::new(AtomicUsize::new(0));
    let bash: Arc<dyn Tool> = Arc::new(StaticTool {
        name: "bash".into(),
        calls: calls.clone(),
    });

    // Hook, der rm -rf blockt.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("guard.rhai"),
        r#"fn on_tool_call(name, input) {
             if name == "bash" && input.command.contains("rm -rf") { return block("nein"); }
             continue_()
           }"#,
    )
    .unwrap();
    let hooks = Box::new(sepp_hooks::RhaiHookHost::from_dirs(&[tmp.path().to_path_buf()]).unwrap());

    let mut session = AgentSession::builder()
        .provider(provider)
        .model(test_model())
        .tools(vec![bash])
        .hooks(hooks)
        .build()
        .unwrap();

    let noop = |_ev: AgentEvent| {};
    session
        .prompt("lösche alles", &noop, CancellationToken::new())
        .await
        .unwrap();

    // Das Tool wurde NICHT ausgeführt.
    assert_eq!(calls.load(SeqCst), 0);
    // Es existiert eine Tool-Result-Message, die als Fehler (geblockt) markiert ist.
    let blocked = session.state().messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, sepp_core::ContentBlock::ToolResult { is_error, content, .. }
                if *is_error && content.iter().any(|c| matches!(c, sepp_core::ContentBlock::Text { text } if text.contains("nein"))))
        })
    });
    assert!(blocked);
}

#[tokio::test]
async fn compact_after_branch_keeps_summary_on_active_path() {
    // Zwei Turns, dann eine Compaction-Zusammenfassung.
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::from(vec![
            text_turn("A1"),
            text_turn("A2"),
            text_turn("SUMMARY"),
        ])),
    });
    let store = Box::new(sepp_session::InMemorySessionStore::new());
    let mut session = AgentSession::builder()
        .provider(provider)
        .model(test_model())
        .tools(vec![])
        .session(store)
        .build()
        .unwrap();

    let noop = |_ev: AgentEvent| {};
    session
        .prompt("erste", &noop, CancellationToken::new())
        .await
        .unwrap();
    session
        .prompt("zweite", &noop, CancellationToken::new())
        .await
        .unwrap();

    // Auf den ersten Eintrag verzweigen → Leaf != entries().last().
    let first_id = session.session().unwrap().entries()[0].id.clone();
    session.session_mut().unwrap().branch(&first_id).unwrap();
    session.reload_from_session();

    session.compact(None).await.unwrap();

    // Die aktive Conversation muss mit der Zusammenfassung beginnen (nicht verloren gehen).
    let msgs = session.session().unwrap().path_messages();
    assert!(!msgs.is_empty(), "Zusammenfassung ging verloren");
    assert!(matches!(&msgs[0].content[0],
        sepp_core::ContentBlock::Text { text } if text.contains("SUMMARY")));
}
