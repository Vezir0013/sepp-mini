//! Moonshot AI (Kimi) spricht den OpenAI-kompatiblen Chat-Completions-Stream und teilt sich
//! deshalb den Decoder mit dem OpenAI-Adapter. Der dedizierte Connector (`--provider moonshot`)
//! ruft denselben `decode_openai_sse`-Pfad auf; nur Identität und Fehlertexte sind getrennt.
//! Das Fixture ist ein synthetischer, repräsentativer Kimi-Stream und dient als
//! Regressions-Anker dafür, dass Moonshots Drahtformat korrekt dekodiert.
#![cfg(feature = "openai")]

use sepp_provider::{decode_openai_sse, StopReason, StreamEvent};

#[test]
fn decodes_moonshot_kimi_stream() {
    let raw = include_bytes!("fixtures/moonshot_basic.sse");
    let events = decode_openai_sse(raw);

    // Erstes Event ist MessageStart, letztes ist MessageStop{tool_use}.
    assert!(matches!(events.first(), Some(StreamEvent::MessageStart)));
    assert!(matches!(
        events.last(),
        Some(StreamEvent::MessageStop {
            stop_reason: StopReason::ToolUse
        })
    ));

    // Kimi streamt sein Denken in `reasoning_content` → wird als ThinkingDelta abgebildet.
    let thinking: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ThinkingDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, "Der Nutzer will das Datum.");

    // Das leere `content:""` im Rollen-Delta darf KEIN TextDelta erzeugen.
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hallo von Kimi");

    // Tool-Call inkrementell: id + name zuerst, Argumente in Folge-Deltas.
    assert!(events.iter().any(|e| matches!(e,
        StreamEvent::ToolUseStart { id, name } if id == "call_moonshot_1" && name == "get_date")));
    let args: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolUseInputDelta { partial_json, .. } => Some(partial_json.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(args, "{\"tz\":\"UTC\"}");

    let usage = events.iter().find_map(|e| match e {
        StreamEvent::Usage(u) => Some(*u),
        _ => None,
    });
    assert_eq!(usage.map(|u| u.output_tokens), Some(11));

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

#[test]
fn decodes_moonshot_stream_without_reasoning_content() {
    // kimi-k3 streamt `reasoning_content` (live bestätigt), das ChoiceDelta-Schema der
    // API-Referenz listet das Feld aber nicht — bei anderen/künftigen Moonshot-Modellen kann es
    // also fehlen. Dann darf kein ThinkingDelta entstehen und der Text muss trotzdem vollständig
    // durchkommen; sonst hinge die Antwort an einem undokumentierten Detail.
    let raw = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hallo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" von Kimi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n",
        "data: [DONE]\n\n",
    );
    let events = decode_openai_sse(raw.as_bytes());

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, StreamEvent::ThinkingDelta { .. })),
        "{events:?}"
    );
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hallo von Kimi");
    assert!(
        matches!(
            events.last(),
            Some(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn
            })
        ),
        "{events:?}"
    );
}
