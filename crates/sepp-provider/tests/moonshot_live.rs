//! Live-Smoke-Test für den Moonshot-Connector ([`MoonshotProvider`]). Macht **einen echten,
//! minimalen** Chat-Completions-Call gegen api.moonshot.ai und prüft, dass ein sauberer Stream
//! zurückkommt (kein `StreamEvent::Error`, MessageStart … terminaler MessageStop, etwas Text).
//!
//! Per Default geskippt (`#[ignore]`). Läuft nur über `just test-live`
//! (`SEPP_LIVE_TESTS=1 cargo test --workspace -- --include-ignored`) UND mit gesetztem
//! `MOONSHOT_API_KEY`. Fehlt einer der beiden Schalter, ist der Test ein No-op (kein Fehler),
//! damit `--include-ignored` ohne Key/Guthaben nicht rot wird. Endpunkt via `MOONSHOT_BASE_URL`
//! überschreibbar (z. B. China-Region).
//!
//! Der Lauf gibt zusätzlich die Thinking-Länge aus. Hintergrund: Moonshots Quickstart nennt
//! gestreamte `reasoning_content`-Deltas, das ChoiceDelta-Schema der API-Referenz listet das Feld
//! nicht. Ein Live-Lauf gegen `kimi-k3` hat bestätigt, dass die Deltas **kommen** — bleibt die
//! Ausgabe hier bei 0 Zeichen, hat sich das Drahtformat geändert und der Mapper braucht ein
//! weiteres Feld (`openai.rs`, `reasoning_content`/`reasoning`).
#![cfg(feature = "moonshot")]

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use sepp_core::{Message, ThinkingLevel};
use sepp_provider::{models, CompletionRequest, MoonshotProvider, Provider, StreamEvent};

#[tokio::test]
#[ignore = "Live-Netz-Call gegen api.moonshot.ai; nur via SEPP_LIVE_TESTS=1 + MOONSHOT_API_KEY"]
async fn moonshot_live_minimal_call() {
    // Doppelter Riegel: Selbst mit `--include-ignored` nur laufen, wenn ausdrücklich gewollt UND
    // ein Key da ist — sonst stiller Skip statt Fehlschlag.
    if std::env::var("SEPP_LIVE_TESTS").ok().as_deref() != Some("1") {
        eprintln!("moonshot_live_minimal_call: SEPP_LIVE_TESTS != 1 — übersprungen");
        return;
    }
    if std::env::var("MOONSHOT_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .is_none()
    {
        eprintln!("moonshot_live_minimal_call: MOONSHOT_API_KEY nicht gesetzt — übersprungen");
        return;
    }

    let provider = MoonshotProvider::from_env().expect("MoonshotProvider::from_env");
    // Identitäts-Garantie auch live: ein Moonshot-Aufruf firmiert nie unter „openai".
    assert_eq!(provider.name(), "moonshot");

    let model = models::find_model("kimi-k3").expect("kimi-k3 in der Registry");
    let messages = vec![Message::user_text("Antworte mit genau einem Wort: pong")];
    let req = CompletionRequest {
        model: &model,
        system: None,
        messages: &messages,
        tools: &[],
        // `Off` → `reasoning_effort: "low"`, die billigste Stufe. Abschalten geht bei Kimi nicht.
        thinking: ThinkingLevel::Off,
        // Deutlich großzügiger als bei den anderen Live-Tests (dort 32): Kimi denkt immer, und
        // das Denken zählt gegen dasselbe Budget. Bei 32 Tokens wäre `finish_reason: "length"`
        // erreicht, bevor überhaupt Antworttext entsteht — der Test würde ein Budget-Artefakt
        // messen statt der Konnektivität.
        max_tokens: 2048,
    };

    let cancel = CancellationToken::new();
    let stream = provider
        .stream(req, cancel)
        .await
        .expect("Moonshot-Stream öffnen (HTTP-Status ok? Key/Guthaben/Endpunkt prüfen)");
    let events: Vec<StreamEvent> = stream.collect().await;

    // Kein Error-Event im Stream — Fehlertexte tragen dank dediziertem Connector `moonshot:`.
    let errors: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Error { message } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        errors.is_empty(),
        "Moonshot lieferte Error-Event(s): {errors:?}"
    );

    // Sauberer Rahmen: MessageStart zuerst, terminaler MessageStop zuletzt.
    assert!(
        matches!(events.first(), Some(StreamEvent::MessageStart)),
        "kein MessageStart am Anfang: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(StreamEvent::MessageStop { .. })),
        "kein terminaler MessageStop: {events:?}"
    );

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let thinking: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ThinkingDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    // Strikt auf nicht-leeren Text: „alles landet im Reasoning, stdout bleibt leer" war exakt
    // der Ollama-Fehler aus 0.1.16. Wird dieser Assert rot, ist das eine echte Erkenntnis über
    // Kimi und kein Testartefakt — dann Budget/Prompt anpassen und den Grund hier notieren.
    assert!(
        !text.trim().is_empty(),
        "leere Antwort von Moonshot (alles im Reasoning? {} Zeichen): {events:?}",
        thinking.len()
    );
    eprintln!(
        "moonshot_live_minimal_call OK — Antwort: {:?}, Thinking: {} Zeichen \
         (0 ⇒ Kimi streamt kein reasoning_content)",
        text.trim(),
        thinking.len()
    );
}
