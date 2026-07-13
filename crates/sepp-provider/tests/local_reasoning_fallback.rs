//! Fallback-Verhalten des Local-Dialekts: Endpunkte, die `reasoning_effort` nicht kennen
//! (Ollama < 0.18: „invalid think value"; vLLM je nach Modell), lehnen den Request mit 4xx
//! ab — der Provider muss dann einmal ohne das Feld wiederholen und es für den Rest der
//! Sitzung weglassen, statt `--provider local` komplett zu brechen. Getestet gegen einen
//! Mini-HTTP-Server auf 127.0.0.1 (kein echtes Netz, keine Keys).

#![cfg(feature = "openai")]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use sepp_core::{Model, ThinkingLevel};
use sepp_provider::{CompletionRequest, OpenAiDialect, OpenAiProvider, Provider, StreamEvent};

fn local_model() -> Model {
    Model {
        id: "qwen-test".into(),
        provider: "openai".into(),
        display_name: "(custom)".into(),
        context_window: 128_000,
        max_output_tokens: 8192,
        supports_reasoning: true,
        supports_images: false,
    }
}

/// Minimaler SSE-Erfolgsbody im OpenAI-Chat-Completions-Drahtformat.
fn sse_body() -> String {
    concat!(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
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
/// und jeden empfangenen Request-Body mitschreibt. `Connection: close` erzwingt je Request
/// eine frische Verbindung — so bleibt der Ablauf strikt sequenziell.
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

fn provider_for(addr: SocketAddr) -> OpenAiProvider {
    OpenAiProvider::new(None, format!("http://{addr}/v1")).with_dialect(OpenAiDialect::Local)
}

async fn run_stream(p: &OpenAiProvider, model: &Model) -> sepp_core::Result<Vec<StreamEvent>> {
    let req = CompletionRequest {
        model,
        system: None,
        messages: &[],
        tools: &[],
        thinking: ThinkingLevel::Off,
        max_tokens: 8192,
    };
    let s = p.stream(req, CancellationToken::new()).await?;
    Ok(s.collect().await)
}

#[tokio::test]
async fn falls_back_without_reasoning_effort_and_remembers_rejection() {
    let reject = "{\"error\":{\"message\":\"invalid think value: \\\"none\\\"\"}}".to_string();
    let (addr, bodies) =
        spawn_server(vec![(400, reject), (200, sse_body()), (200, sse_body())]).await;
    let p = provider_for(addr);
    let model = local_model();

    // Erster Turn: 400 mit Feld → Retry ohne Feld muss durchgehen und Events liefern.
    let events = run_stream(&p, &model).await.expect("fallback stream");
    assert!(events
        .iter()
        .any(|e| matches!(e, StreamEvent::TextDelta { text } if text == "ok")));
    assert!(events
        .iter()
        .any(|e| matches!(e, StreamEvent::MessageStop { .. })));

    // Zweiter Turn: die Ablehnung ist gemerkt — das Feld wird gar nicht mehr gesendet.
    run_stream(&p, &model).await.expect("second stream");

    let seen = bodies.lock().expect("lock");
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert!(seen[0].contains("reasoning_effort"), "{}", seen[0]);
    assert!(!seen[1].contains("reasoning_effort"), "{}", seen[1]);
    assert!(!seen[2].contains("reasoning_effort"), "{}", seen[2]);
}

#[tokio::test]
async fn server_errors_do_not_trigger_fallback() {
    // 5xx hat nichts mit dem Feld zu tun: kein Retry, Fehler geht durch, und der nächste
    // Request sendet das Feld unverändert (kein fälschlich gesetztes Ablehnungs-Flag).
    let (addr, bodies) = spawn_server(vec![
        (500, "{\"error\":\"boom\"}".to_string()),
        (200, sse_body()),
    ])
    .await;
    let p = provider_for(addr);
    let model = local_model();

    let err = run_stream(&p, &model).await.expect_err("5xx must fail");
    assert!(err.to_string().contains("500"), "{err}");

    run_stream(&p, &model).await.expect("second stream");
    let seen = bodies.lock().expect("lock");
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(seen[1].contains("reasoning_effort"), "{}", seen[1]);
}

#[tokio::test]
async fn failed_retry_keeps_field_for_next_request() {
    // Schlägt auch der Retry fehl (transienter 4xx, z. B. kaputtes Modell), darf die
    // Thinking-Unterdrückung nicht dauerhaft deaktiviert werden.
    let reject = "{\"error\":\"bad\"}".to_string();
    let (addr, bodies) = spawn_server(vec![
        (400, reject.clone()),
        (400, reject),
        (200, sse_body()),
    ])
    .await;
    let p = provider_for(addr);
    let model = local_model();

    run_stream(&p, &model)
        .await
        .expect_err("double 4xx must fail");

    run_stream(&p, &model).await.expect("third stream");
    let seen = bodies.lock().expect("lock");
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert!(seen[0].contains("reasoning_effort"), "{}", seen[0]);
    assert!(!seen[1].contains("reasoning_effort"), "{}", seen[1]);
    assert!(seen[2].contains("reasoning_effort"), "{}", seen[2]);
}
