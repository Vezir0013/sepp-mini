//! z.ai (Zhipu/GLM) spricht den OpenAI-kompatiblen Chat-Completions-Stream und teilt sich
//! deshalb den Decoder mit dem OpenAI-Adapter (`--provider zai` baut einen `OpenAiProvider`
//! mit z.ai-base_url). Dieses Fixture ist ein synthetischer, repräsentativer GLM-Stream
//! (das Live-Test-Konto hatte kein Guthaben) und dient als Regressions-Anker dafür, dass
//! z.ais Drahtformat über `decode_openai_sse` korrekt dekodiert.
#![cfg(feature = "openai")]

use sepp_provider::{decode_openai_sse, StopReason, StreamEvent};

#[test]
fn decodes_zai_glm_stream() {
    let raw = include_bytes!("fixtures/zai_basic.sse");
    let events = decode_openai_sse(raw);

    // Erstes Event ist MessageStart, letztes ist MessageStop{tool_use}.
    assert!(matches!(events.first(), Some(StreamEvent::MessageStart)));
    assert!(matches!(
        events.last(),
        Some(StreamEvent::MessageStop {
            stop_reason: StopReason::ToolUse
        })
    ));

    // GLM streamt sein Denken in `reasoning_content` → wird als ThinkingDelta abgebildet.
    let thinking: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ThinkingDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, "Der Nutzer will die Zeit.");

    // Text-Deltas konkatenieren zu "Hallo von GLM".
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hallo von GLM");

    // GLM liefert den Tool-Call in EINEM Delta (id + name + vollständige Argumente zusammen).
    assert!(events.iter().any(|e| matches!(e,
        StreamEvent::ToolUseStart { id, name } if id == "call_zai_1" && name == "get_time")));
    let args: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolUseInputDelta { partial_json, .. } => Some(partial_json.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(args, "{\"tz\": \"UTC\"}");

    // Usage wird aus dem abschließenden usage-Chunk übernommen.
    let usage = events.iter().find_map(|e| match e {
        StreamEvent::Usage(u) => Some(*u),
        _ => None,
    });
    assert_eq!(usage.map(|u| u.output_tokens), Some(9));

    // Ordering-Invariante: ToolUseStop vor Usage vor MessageStop.
    let stop = events
        .iter()
        .position(|e| matches!(e, StreamEvent::ToolUseStop { .. }));
    let usage_pos = events
        .iter()
        .position(|e| matches!(e, StreamEvent::Usage(_)));
    let msgstop = events
        .iter()
        .position(|e| matches!(e, StreamEvent::MessageStop { .. }));
    assert!(stop < usage_pos && usage_pos < msgstop, "{events:?}");
}
