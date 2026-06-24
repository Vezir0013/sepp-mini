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
