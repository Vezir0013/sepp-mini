//! Hilfen der Compaction — rein, ohne I/O, ohne Provider: die Kürzung des Verlaufs für einen
//! zweiten Zusammenfassungsversuch, der harte Schnitt als letzte Stufe, die Heuristik, die einen
//! Kontextüberlauf von anderen Provider-Fehlern trennt, und die Zuordnung eines Schnitts zu
//! Store-Einträgen. Der Ablauf selbst steht in [`crate::AgentSession::compact`].

use std::collections::HashMap;

use sepp_core::{ContentBlock, Message, Role};
use sepp_session::{Entry, EntryId, EntryPayload};

/// Wie viele der jüngsten Nachrichten der harte Schnitt mindestens behält.
pub const KEEP_TAIL: usize = 2;
const TEXT_HEAD: usize = 2_000;
const TEXT_TAIL: usize = 1_000;
const RESULT_HEAD: usize = 700;
const RESULT_TAIL: usize = 300;

/// Sieht der Provider-Fehler nach einem zu langen Kontext aus? Die Adapter formatieren
/// `<label>: HTTP <status>: <body>`; Anthropic antwortet 400 „prompt is too long", OpenAI-
/// kompatible 400 `context_length_exceeded`, manche Proxys 413. Ein Netz- oder Schlüsselfehler
/// (Verbindung, 401, 5xx) ist keiner — dort wäre ein Schnitt Datenverlust ohne Gewinn.
pub fn looks_like_context_overflow(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    if m.contains("http 400") || m.contains("http 413") {
        return true;
    }
    [
        "too long",
        "context length",
        "context_length",
        "maximum context",
        "context window",
        "token limit",
        "too many tokens",
        "request too large",
    ]
    .iter()
    .any(|k| m.contains(k))
}

/// Kürzt in der Mitte, an Zeichengrenzen; kurze Texte bleiben unverändert.
pub fn truncate_middle(s: &str, head: usize, tail: usize) -> String {
    let total = s.chars().count();
    if total <= head + tail + 24 {
        return s.to_string();
    }
    let h: String = s.chars().take(head).collect();
    let t: String = s
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{h}\n…[{} Zeichen gekürzt]…\n{t}", total - head - tail)
}

/// Der Verlauf für den zweiten Zusammenfassungsversuch: Thinking fällt weg, lange Texte und
/// Werkzeug-Ergebnisse werden in der Mitte gekürzt, Bilder zu ihrer Beschreibung. Die
/// Struktur bleibt — jedes `tool_use` behält sein `tool_result`, sonst lehnte der Anbieter
/// auch die Zusammenfassung ab.
pub fn reduce_for_summary(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|m| Message {
            role: m.role,
            usage: None,
            content: m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Thinking { .. } => None,
                    ContentBlock::Text { text } => Some(ContentBlock::text(truncate_middle(
                        text, TEXT_HEAD, TEXT_TAIL,
                    ))),
                    ContentBlock::Image { source } => Some(ContentBlock::text(source.describe())),
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        Some(ContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            is_error: *is_error,
                            content: content
                                .iter()
                                .map(|c| match c {
                                    ContentBlock::Text { text } => ContentBlock::text(
                                        truncate_middle(text, RESULT_HEAD, RESULT_TAIL),
                                    ),
                                    ContentBlock::Image { source } => {
                                        ContentBlock::text(source.describe())
                                    }
                                    other => other.clone(),
                                })
                                .collect(),
                        })
                    }
                    other => Some(other.clone()),
                })
                .collect(),
        })
        .collect()
}

/// Ab welchem Index der Verlauf beim harten Schnitt behalten wird: die größte Stelle, die
/// mindestens `keep` Nachrichten übrig lässt, mindestens eine entfernt und an der ein Schnitt
/// kein `tool_use` ohne `tool_result` hinterlässt — also eine Assistant-Nachricht (das Paar
/// `tool_use`/`tool_result` bleibt zusammen; davor steht ohnehin der Hinweis als
/// User-Nachricht) oder eine User-Nachricht ohne Werkzeug-Ergebnis. `None`, wenn nichts zu
/// schneiden ist.
pub fn hard_cut_start(messages: &[Message], keep: usize) -> Option<usize> {
    let len = messages.len();
    if len <= keep {
        return None;
    }
    (1..=len - keep).rev().find(|&i| is_cut_point(&messages[i]))
}

fn is_cut_point(m: &Message) -> bool {
    match m.role {
        Role::Assistant => true,
        Role::User => !m
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. })),
        _ => false,
    }
}

/// Der Store-Eintrag, bis zu dem ein harter Schnitt ersetzt: vom Leaf rückwärts die letzten
/// `keep` Message-Einträge überspringen (Custom-Einträge wie `guard` zählen nicht), der nächste
/// Message-Eintrag ist die letzte entfernte Nachricht. `None`, wenn vorher eine Compaction
/// liegt (davor ist schon ersetzt) oder nichts zu entfernen bleibt — dann wird nichts
/// persistiert.
pub fn replaced_until_for_tail(
    entries: &[Entry],
    leaf: Option<&EntryId>,
    keep: usize,
) -> Option<EntryId> {
    let index: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.as_str(), i))
        .collect();
    let mut cur = leaf.and_then(|l| index.get(l.as_str()).copied());
    let mut seen = 0usize;
    while let Some(i) = cur {
        let e = &entries[i];
        match &e.payload {
            EntryPayload::Message { .. } => {
                if seen >= keep {
                    return Some(e.id.clone());
                }
                seen += 1;
            }
            EntryPayload::Compaction { .. } => return None,
            EntryPayload::Custom { .. } => {}
        }
        cur = e
            .parent_id
            .as_ref()
            .and_then(|p| index.get(p.as_str()).copied());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use sepp_session::{InMemorySessionStore, SessionStore};

    #[test]
    fn overflow_heuristic_separates_context_errors_from_the_rest() {
        assert!(looks_like_context_overflow(
            "anthropic: HTTP 400: prompt is too long"
        ));
        assert!(looks_like_context_overflow(
            "openai: HTTP 400: This model's maximum context length is 8192 tokens"
        ));
        assert!(looks_like_context_overflow(
            "local: HTTP 413: Request Entity Too Large"
        ));
        assert!(!looks_like_context_overflow(
            "anthropic: HTTP 401: invalid x-api-key"
        ));
        assert!(!looks_like_context_overflow(
            "anthropic: HTTP 529: overloaded"
        ));
        assert!(!looks_like_context_overflow(
            "anthropic request: connection refused"
        ));
    }

    #[test]
    fn truncate_middle_is_char_safe_and_leaves_short_text_alone() {
        assert_eq!(truncate_middle("kurz", 2, 1), "kurz");
        let long: String = "äöü".repeat(100);
        let t = truncate_middle(&long, 6, 3);
        assert!(t.starts_with("äöüäöü\n…["));
        assert!(t.ends_with("]…\näöü"), "{t}");
        assert!(t.contains("291 Zeichen gekürzt"));
    }

    #[test]
    fn reduce_keeps_pairs_drops_thinking_and_shortens_results() {
        let big = "x".repeat(5_000);
        let msgs = vec![
            Message::user_text("los"),
            Message::assistant(vec![
                ContentBlock::Thinking {
                    text: "hmm".into(),
                    signature: None,
                },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": "a" }),
                },
            ]),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: vec![ContentBlock::text(big.clone())],
                    is_error: false,
                }],
                usage: None,
            },
        ];
        let r = reduce_for_summary(&msgs);
        assert_eq!(r.len(), 3);
        assert_eq!(r[1].content.len(), 1, "Thinking weg, ToolUse bleibt");
        assert!(matches!(&r[1].content[0], ContentBlock::ToolUse { id, .. } if id == "t1"));
        match &r[2].content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "t1");
                let ContentBlock::Text { text } = &content[0] else {
                    panic!()
                };
                assert!(text.len() < 1_200, "{}", text.len());
                assert!(text.contains("gekürzt"));
            }
            other => panic!("{other:?}"),
        }
    }

    fn tool_pair(id: &str) -> [Message; 2] {
        [
            Message::assistant(vec![ContentBlock::ToolUse {
                id: id.into(),
                name: "t".into(),
                input: serde_json::json!({}),
            }]),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.into(),
                    content: vec![ContentBlock::text("r")],
                    is_error: false,
                }],
                usage: None,
            },
        ]
    }

    #[test]
    fn hard_cut_never_orphans_a_tool_result() {
        // u, a(t1), u(r1), a(t2), u(r2), a(text)
        let mut msgs = vec![Message::user_text("los")];
        msgs.extend(tool_pair("t1"));
        msgs.extend(tool_pair("t2"));
        msgs.push(Message::assistant(vec![ContentBlock::text("fertig")]));
        // keep 2: Kandidat 4 ist u(r2) — kein Schnittpunkt; 3 ist a(t2) → Paar bleibt zusammen.
        assert_eq!(hard_cut_start(&msgs, 2), Some(3));
        assert_eq!(hard_cut_start(&msgs, 5), Some(1));
        assert_eq!(hard_cut_start(&msgs, 6), None);
        assert_eq!(hard_cut_start(&msgs[..2], 2), None);
        // Ein reiner Text-Verlauf schneidet vor der letzten User-Nachricht.
        let plain = vec![
            Message::user_text("a"),
            Message::assistant(vec![ContentBlock::text("b")]),
            Message::user_text("c"),
            Message::assistant(vec![ContentBlock::text("d")]),
        ];
        assert_eq!(hard_cut_start(&plain, 2), Some(2));
    }

    #[test]
    fn replaced_until_skips_custom_entries_and_stops_at_a_compaction() {
        let mut store = InMemorySessionStore::new();
        let msg = |t: &str| EntryPayload::Message {
            message: Message::user_text(t),
        };
        let u1 = store.append(msg("u1")).unwrap();
        let a1 = store.append(msg("a1")).unwrap();
        store
            .append(EntryPayload::Custom {
                kind: "guard".into(),
                data: serde_json::json!({}),
            })
            .unwrap();
        let _u2 = store.append(msg("u2")).unwrap();
        let _a2 = store.append(msg("a2")).unwrap();
        assert_eq!(
            replaced_until_for_tail(store.entries(), store.leaf(), 2).as_deref(),
            Some(a1.as_str())
        );
        assert_eq!(
            replaced_until_for_tail(store.entries(), store.leaf(), 3).as_deref(),
            Some(u1.as_str())
        );
        assert_eq!(
            replaced_until_for_tail(store.entries(), store.leaf(), 4),
            None
        );
        // Nach einer Compaction ist davor nichts mehr zu ersetzen.
        store
            .append(EntryPayload::Compaction {
                summary: "s".into(),
                replaced_until: a1.clone(),
            })
            .unwrap();
        let _a3 = store.append(msg("a3")).unwrap();
        assert_eq!(
            replaced_until_for_tail(store.entries(), store.leaf(), 1),
            None
        );
        // Und `path_messages` liefert nach einem persistierten Schnitt genau die Tail-Messages.
        let path = store.path_messages();
        assert_eq!(path.len(), 4, "summary + u2 + a2 + a3: {path:?}");
    }
}
