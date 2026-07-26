//! Zwei Garantien des Moonshot-Connectors, die sich nur am Draht zeigen — geprüft gegen einen
//! Mini-HTTP-Server auf 127.0.0.1 (kein echtes Netz, keine Keys):
//!
//! 1. **Genau ein Request pro Turn.** Anders als [`sepp_provider::OpenAiProvider`] wiederholt
//!    dieser Connector einen 4xx-Request NICHT ohne `reasoning_effort`. Auf einer Bezahl-API
//!    träfe ein blinder 4xx-Retry auch 401 und 429. Der Server hier hätte einen Retry
//!    beantwortet — dass trotzdem nur ein Body ankommt, ist die eigentliche Aussage.
//! 2. **Das Output-Budget geht als `max_completion_tokens` raus**, nicht als das bei Moonshot
//!    deprecated `max_tokens`.
//!
//! Der HTTP-Harness ist bewusst eine schlanke Kopie aus `local_reasoning_fallback.rs` statt
//! eines geteilten Moduls — der bestehende Test bleibt dadurch unangetastet.

#![cfg(feature = "moonshot")]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use sepp_core::{Model, ThinkingLevel};
use sepp_provider::{CompletionRequest, MoonshotProvider, Provider, StreamEvent};

fn kimi_model() -> Model {
    Model {
        id: "kimi-k3".into(),
        provider: "moonshot".into(),
        display_name: "Kimi K3".into(),
        context_window: 1_048_576,
        max_output_tokens: 32_768,
        supports_reasoning: true,
        supports_images: false,
    }
}

/// Minimaler SSE-Erfolgsbody im OpenAI-Chat-Completions-Drahtformat.
fn sse_body() -> String {
    concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    )
    .to_string()
}

/// Liest genau EINEN HTTP-Request (Header + Content-Length-Body) und liefert den Body.
async fn read_request_body(sock: &mut TcpStream) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = sock.read(&mut tmp).await.expect("read request");
        if n == 0 {
            return String::new();
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&buf[..pos]).into_owned();
            let content_length: usize = header
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.eq_ignore_ascii_case("content-length") {
                        v.trim().parse().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let body_start = pos + 4;
            while buf.len() < body_start + content_length {
                let n = sock.read(&mut tmp).await.expect("read body");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let end = (body_start + content_length).min(buf.len());
            return String::from_utf8_lossy(&buf[body_start..end]).into_owned();
        }
    }
}

/// Startet einen Server, der die geplanten Antworten (Status, Body) in Reihenfolge ausliefert
/// und jeden empfangenen Request-Body mitschreibt.
async fn spawn_server(responses: Vec<(u16, String)>) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    tokio::spawn(async move {
        for (status, body) in responses {
            let (mut sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => return,
            };
            let req_body = read_request_body(&mut sock).await;
            seen.lock().expect("lock").push(req_body);
            let (phrase, content_type) = match status {
                200 => ("OK", "text/event-stream"),
                400 => ("Bad Request", "application/json"),
                _ => ("Error", "application/json"),
            };
            let resp = format!(
                "HTTP/1.1 {status} {phrase}\r\nContent-Type: {content_type}\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (addr, bodies)
}

async fn run_stream(
    p: &MoonshotProvider,
    model: &Model,
    thinking: ThinkingLevel,
) -> sepp_core::Result<Vec<StreamEvent>> {
    let req = CompletionRequest {
        model,
        system: None,
        messages: &[],
        tools: &[],
        thinking,
        max_tokens: 32_768,
    };
    let s = p.stream(req, CancellationToken::new()).await?;
    Ok(s.collect().await)
}

#[tokio::test]
async fn does_not_retry_on_client_error() {
    // Der Server hätte einen Retry mit 200 beantwortet — genau deshalb ist `seen.len() == 1`
    // aussagekräftig: der Connector versucht es gar nicht erst erneut.
    let reject = "{\"error\":{\"message\":\"invalid_request\",\"type\":\"invalid_request_error\"}}"
        .to_string();
    let (addr, bodies) = spawn_server(vec![(400, reject), (200, sse_body())]).await;
    let p = MoonshotProvider::new(Some("test-key".into()), format!("http://{addr}/v1"));

    let err = run_stream(&p, &kimi_model(), ThinkingLevel::Off)
        .await
        .expect_err("400 muss durchschlagen");
    // Identität: ein Moonshot-Fehler darf nicht als „openai" erscheinen.
    assert!(err.to_string().contains("moonshot"), "{err}");
    assert!(err.to_string().contains("400"), "{err}");

    let seen = bodies.lock().expect("lock");
    assert_eq!(
        seen.len(),
        1,
        "kein Retry auf 4xx (401/429 dürfen sich nicht verdoppeln): {seen:?}"
    );
}

#[tokio::test]
async fn sends_max_completion_tokens_and_reasoning_effort() {
    let (addr, bodies) = spawn_server(vec![(200, sse_body())]).await;
    let p = MoonshotProvider::new(Some("test-key".into()), format!("http://{addr}/v1"));

    let events = run_stream(&p, &kimi_model(), ThinkingLevel::Off)
        .await
        .expect("stream");
    assert!(events
        .iter()
        .any(|e| matches!(e, StreamEvent::TextDelta { text } if text == "ok")));

    let seen = bodies.lock().expect("lock");
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(seen[0].contains("max_completion_tokens"), "{}", seen[0]);
    // Mit Anführungszeichen prüfen: ein nacktes `contains("max_tokens")` würde auch im
    // Substring von `max_completion_tokens` anschlagen und wäre damit wertlos.
    assert!(!seen[0].contains("\"max_tokens\""), "{}", seen[0]);
    // Thinking Off → billigste Stufe, NICHT das Weglassen des Feldes (das hieße Default `max`).
    assert!(
        seen[0].contains("\"reasoning_effort\":\"low\""),
        "{}",
        seen[0]
    );
}
