//! Live-Smoke-Test für den z.ai-Connector ([`ZaiProvider`]). Macht **einen echten, minimalen**
//! Chat-Completions-Call gegen api.z.ai und prüft, dass ein sauberer Stream zurückkommt
//! (kein `StreamEvent::Error`, MessageStart … terminaler MessageStop, etwas Text).
//!
//! Per Default geskippt (`#[ignore]`). Läuft nur über `just test-live`
//! (`SEPP_LIVE_TESTS=1 cargo test --workspace -- --include-ignored`) UND mit gesetztem
//! `ZAI_API_KEY`. Fehlt einer der beiden Schalter, ist der Test ein No-op (kein Fehler), damit
//! `--include-ignored` ohne Key/Guthaben nicht rot wird. Endpunkt via `ZAI_BASE_URL`
//! überschreibbar (z. B. China-Region).
#![cfg(feature = "zai")]

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use sepp_core::{Message, ThinkingLevel};
use sepp_provider::{models, CompletionRequest, Provider, StreamEvent, ZaiProvider};

#[tokio::test]
#[ignore = "Live-Netz-Call gegen api.z.ai; nur via SEPP_LIVE_TESTS=1 + ZAI_API_KEY"]
async fn zai_live_minimal_call() {
    // Doppelter Riegel: Selbst mit `--include-ignored` nur laufen, wenn ausdrücklich gewollt UND
    // ein Key da ist — sonst stiller Skip statt Fehlschlag.
    if std::env::var("SEPP_LIVE_TESTS").ok().as_deref() != Some("1") {
        eprintln!("zai_live_minimal_call: SEPP_LIVE_TESTS != 1 — übersprungen");
        return;
    }
    if std::env::var("ZAI_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .is_none()
    {
        eprintln!("zai_live_minimal_call: ZAI_API_KEY nicht gesetzt — übersprungen");
        return;
    }

    let provider = ZaiProvider::from_env().expect("ZaiProvider::from_env");
    // Identitäts-Garantie auch live: ein z.ai-Aufruf firmiert nie unter „openai".
    assert_eq!(provider.name(), "zai");

    // Günstigstes/kleinstes registriertes GLM-Modell, Reasoning aus, winziges Output-Budget —
    // hält den Smoke-Test billig und schnell.
    let model = models::find_model("glm-4.5-flash").expect("glm-4.5-flash in der Registry");
    let messages = vec![Message::user_text("Antworte mit genau einem Wort: pong")];
    let req = CompletionRequest {
        model: &model,
        system: None,
        messages: &messages,
        tools: &[],
        thinking: ThinkingLevel::Off,
        max_tokens: 32,
    };

    let cancel = CancellationToken::new();
    let stream = provider
        .stream(req, cancel)
        .await
        .expect("z.ai-Stream öffnen (HTTP-Status ok? Key/Guthaben/Endpunkt prüfen)");
    let events: Vec<StreamEvent> = stream.collect().await;

    // Kein Error-Event im Stream — Fehlertexte tragen dank dediziertem Connector `zai:`.
    let errors: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Error { message } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        errors.is_empty(),
        "z.ai lieferte Error-Event(s): {errors:?}"
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

    // Es sollte zumindest etwas Text zurückkommen.
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        !text.trim().is_empty(),
        "leere Antwort von z.ai: {events:?}"
    );
    eprintln!("zai_live_minimal_call OK — Antwort: {:?}", text.trim());
}
