//! Parser-Test gegen ein aufgezeichnetes Anthropic-SSE-Fixture (ohne Netz).

use sepp_provider::{decode_anthropic_sse, StopReason, StreamEvent};

#[test]
fn decodes_text_and_tool_use_stream() {
    let raw = include_bytes!("fixtures/anthropic_basic.sse");
    let events = decode_anthropic_sse(raw);

    // Erstes Event ist MessageStart, letztes ist MessageStop{tool_use}.
    assert!(matches!(events.first(), Some(StreamEvent::MessageStart)));
    assert!(matches!(
        events.last(),
        Some(StreamEvent::MessageStop {
            stop_reason: StopReason::ToolUse
        })
    ));

    // Text-Deltas konkatenieren zu "Hallo Welt".
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hallo Welt");

    // Tool-Use: Start mit Name, Input-Deltas konkatenieren zu gültigem JSON.
    assert!(events.iter().any(|e| matches!(
        e,
        StreamEvent::ToolUseStart { id, name } if id == "toolu_1" && name == "bash"
    )));
    let tool_input: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolUseInputDelta { id, partial_json } if id == "toolu_1" => {
                Some(partial_json.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(tool_input, "{\"command\":\"ls\"}");
    assert!(events
        .iter()
        .any(|e| matches!(e, StreamEvent::ToolUseStop { id } if id == "toolu_1")));

    // Usage trägt Input- und Output-Tokens.
    let usage = events.iter().find_map(|e| match e {
        StreamEvent::Usage(u) => Some(*u),
        _ => None,
    });
    let usage = usage.expect("Usage-Event erwartet");
    assert_eq!(usage.input_tokens, 42);
    assert_eq!(usage.output_tokens, 15);
}

#[test]
fn decodes_thinking_block_with_signature() {
    // Extended Thinking + Tool-Use: das Denken kommt als thinking_delta-Folge, abgeschlossen
    // von EINEM signature_delta. Die Signatur muss als eigenes Event ankommen — sie wird
    // beim Zurücksenden des Blocks gebraucht (sonst 400 bei der Tool-Use-Fortsetzung).
    let raw = include_bytes!("fixtures/anthropic_thinking.sse");
    let events = decode_anthropic_sse(raw);

    let thinking: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ThinkingDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, "Ich sollte das Verzeichnis listen.");

    // Genau eine Signatur, unverändert übernommen.
    let sigs: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ThinkingSignature { signature } => Some(signature.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(sigs, ["EqQBCgIYAhIMsig123"]);

    // Ordnung: Signatur nach dem letzten ThinkingDelta, vor dem ToolUseStart —
    // so schließt sie im Agent-Loop den richtigen Block ab.
    let last_delta = events
        .iter()
        .rposition(|e| matches!(e, StreamEvent::ThinkingDelta { .. }))
        .unwrap();
    let sig_pos = events
        .iter()
        .position(|e| matches!(e, StreamEvent::ThinkingSignature { .. }))
        .unwrap();
    let tool_start = events
        .iter()
        .position(|e| matches!(e, StreamEvent::ToolUseStart { .. }))
        .unwrap();
    assert!(last_delta < sig_pos && sig_pos < tool_start, "{events:?}");

    assert!(matches!(
        events.last(),
        Some(StreamEvent::MessageStop {
            stop_reason: StopReason::ToolUse
        })
    ));
}
