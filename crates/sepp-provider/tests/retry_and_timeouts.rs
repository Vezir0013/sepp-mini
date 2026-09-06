//! Wiederanlauf und Abbrechbarkeit am Draht — geprüft gegen Mini-HTTP-Server auf 127.0.0.1
//! (kein echtes Netz, keine Keys). Die Rechenregeln dahinter (Backoff, `Retry-After`, welche
//! Statuscodes) sind reine Funktionen und in `src/http.rs` unit-getestet; hier geht es
//! ausschließlich um die Verdrahtung:
//!
//! 1. Ein `429` wird wiederholt, der zweite Versuch kommt durch, und ein `Notice` erklärt dem
//!    Menschen die Verzögerung — **vor** `MessageStart`, wie die Invariante es verlangt.
//! 2. Ein dauerhaft überlasteter Anbieter ergibt nach drei Versuchen einen Fehler, der den
//!    Status nennt.
//! 3. Ein `401` wird **nicht** wiederholt — ein zweiter Call mit falschem Key ist sinnlos.
//! 4. Ctrl+C wirkt in beiden Wartephasen: beim Warten auf die Antwort und während des Backoffs.
//! 5. Ein riesiger Fehler-Body landet gedeckelt in der Meldung, nicht vollständig im Speicher.
//!
//! Damit kein Test echte Sekunden verbringt, senden die Server `Retry-After: 0` — gültig und
//! sofort fällig. Der Harness ist eine bewusste Kopie aus `moonshot_no_retry.rs` (dort in der
//! Modul-Doku als Absicht festgehalten), erweitert um Kopfzeilen und einen schweigenden Modus.

#![cfg(feature = "anthropic")]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use sepp_core::{Model, SeppError, ThinkingLevel};
use sepp_provider::{AnthropicProvider, CompletionRequest, Provider, StreamEvent};

/// Eine geplante Antwort des Test-Servers.
struct Reply {
    status: u16,
    body: String,
    /// Wert der `Retry-After`-Kopfzeile; `Some("0")` heißt „sofort wieder".
    retry_after: Option<&'static str>,
}

impl Reply {
    fn new(status: u16, body: impl Into<String>) -> Self {
        Reply {
            status,
            body: body.into(),
            retry_after: None,
        }
    }

    fn retry_after(mut self, v: &'static str) -> Self {
        self.retry_after = Some(v);
        self
    }
}

fn model() -> Model {
    Model {
        id: "claude-test".into(),
        provider: "anthropic".into(),
        display_name: "Claude Test".into(),
        context_window: 200_000,
        max_output_tokens: 8_192,
        supports_reasoning: false,
        supports_images: true,
    }
}

/// Ein gültiger Anthropic-SSE-Erfolgsbody — dieselbe Aufzeichnung, gegen die der Decoder testet.
fn sse_body() -> String {
    String::from_utf8_lossy(include_bytes!("fixtures/anthropic_basic.sse")).into_owned()
}

/// Liest genau EINEN HTTP-Request (Kopf + Content-Length-Body) und liefert den Body.
async fn read_request_body(sock: &mut TcpStream) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = match sock.read(&mut tmp).await {
            Ok(n) => n,
            Err(_) => return String::new(),
        };
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
                let n = match sock.read(&mut tmp).await {
                    Ok(n) => n,
                    Err(_) => break,
                };
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

/// Startet einen Server, der die geplanten Antworten der Reihe nach ausliefert und jeden
/// empfangenen Request-Body mitschreibt. `Connection: close` erzwingt je Request eine frische
/// Verbindung, damit die Zählung eindeutig ist.
async fn spawn_server(responses: Vec<Reply>) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    tokio::spawn(async move {
        for r in responses {
            let (mut sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => return,
            };
            let req_body = read_request_body(&mut sock).await;
            seen.lock().expect("lock").push(req_body);
            let (phrase, content_type) = match r.status {
                200 => ("OK", "text/event-stream"),
                401 => ("Unauthorized", "application/json"),
                429 => ("Too Many Requests", "application/json"),
                _ => ("Error", "application/json"),
            };
            let extra = match r.retry_after {
                Some(v) => format!("Retry-After: {v}\r\n"),
                None => String::new(),
            };
            let head = format!(
                "HTTP/1.1 {} {phrase}\r\nContent-Type: {content_type}\r\n{extra}\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                r.status,
                r.body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(r.body.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (addr, bodies)
}

/// Ein Server, der die Verbindung annimmt und dann schweigt — der Fall, in dem `send()` auf die
/// Antwortkopfzeilen wartet. `_keep` hält den Socket am Leben; ohne das bekäme der Client EOF.
async fn spawn_silent_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _keep = listener.accept().await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    addr
}

fn provider(addr: SocketAddr) -> AnthropicProvider {
    AnthropicProvider::new("test-key").with_base_url(format!("http://{addr}"))
}

async fn run(
    p: &AnthropicProvider,
    cancel: CancellationToken,
) -> sepp_core::Result<Vec<StreamEvent>> {
    let m = model();
    let req = CompletionRequest {
        model: &m,
        system: None,
        messages: &[],
        tools: &[],
        thinking: ThinkingLevel::Off,
        max_tokens: 1024,
    };
    let s = p.stream(req, cancel).await?;
    Ok(s.collect().await)
}

#[tokio::test]
async fn rate_limit_is_retried_and_announced() {
    let (addr, bodies) = spawn_server(vec![
        Reply::new(429, r#"{"error":{"message":"rate limited"}}"#).retry_after("0"),
        Reply::new(200, sse_body()),
    ])
    .await;

    let events = run(&provider(addr), CancellationToken::new())
        .await
        .expect("der zweite Versuch muss durchkommen");

    // Der Hinweis steht VOR dem Stream — Invariante `Notice* MessageStart …`.
    match events.first() {
        Some(StreamEvent::Notice { text }) => {
            assert!(text.contains("429"), "Status im Hinweis: {text}");
            assert!(
                text.contains("Versuch 2 von 3"),
                "Zähler im Hinweis: {text}"
            );
        }
        other => panic!("erstes Ereignis muss ein Notice sein, war: {other:?}"),
    }
    assert!(
        matches!(events.get(1), Some(StreamEvent::MessageStart)),
        "nach dem Notice folgt MessageStart: {:?}",
        events.get(1)
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::MessageStop { .. })),
        "der Stream muss terminal enden"
    );
    assert_eq!(
        bodies.lock().expect("lock").len(),
        2,
        "genau ein Wiederanlauf"
    );
}

#[tokio::test]
async fn overloaded_provider_fails_after_three_attempts() {
    let overloaded = || Reply::new(529, r#"{"error":{"message":"overloaded"}}"#).retry_after("0");
    let (addr, bodies) = spawn_server(vec![overloaded(), overloaded(), overloaded()]).await;

    let err = run(&provider(addr), CancellationToken::new())
        .await
        .expect_err("dauerhaft überlastet muss scheitern");
    let msg = err.to_string();

    // Das Format `<label>: HTTP <status>: <body>` bleibt erhalten — `sepp-agent` erkennt daran
    // einen zu langen Kontext. Der Versuchszähler hängt nur hinten dran.
    assert!(msg.contains("anthropic: HTTP 529"), "{msg}");
    assert!(msg.contains("nach 3 Versuchen"), "{msg}");
    assert_eq!(
        bodies.lock().expect("lock").len(),
        3,
        "drei Versuche, nicht mehr"
    );
}

#[tokio::test]
async fn bad_key_is_not_retried() {
    // Der Server hätte einen zweiten Versuch mit 200 beantwortet — dass nur ein Request ankommt,
    // ist die eigentliche Aussage.
    let (addr, bodies) = spawn_server(vec![
        Reply::new(401, r#"{"error":{"message":"invalid x-api-key"}}"#),
        Reply::new(200, sse_body()),
    ])
    .await;

    let err = run(&provider(addr), CancellationToken::new())
        .await
        .expect_err("401 muss durchschlagen");
    assert!(err.to_string().contains("HTTP 401"), "{err}");
    assert!(
        !err.to_string().contains("Versuchen"),
        "ein einziger Versuch wird nicht gezählt: {err}"
    );
    assert_eq!(bodies.lock().expect("lock").len(), 1, "kein Wiederanlauf");
}

#[tokio::test]
async fn cancel_during_backoff_aborts_promptly() {
    // `Retry-After: 5` — ohne Abbruchprüfung im Backoff würde der Test fünf Sekunden stehen.
    let (addr, _bodies) = spawn_server(vec![
        Reply::new(503, "busy").retry_after("5"),
        Reply::new(200, sse_body()),
    ])
    .await;

    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c.cancel();
    });

    let started = Instant::now();
    let err = run(&provider(addr), cancel)
        .await
        .expect_err("Abbruch im Backoff muss durchschlagen");
    assert!(
        matches!(err, SeppError::Aborted),
        "Abbruch meldet sich als Aborted, war: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "der Abbruch darf nicht auf Retry-After warten: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn cancel_while_waiting_for_response_aborts() {
    // Der Server nimmt an und schweigt: genau die Phase, in der Ctrl+C bisher wirkungslos war.
    // Ein Lese-Timeout gibt es bewusst nicht — nur der Abbruch beendet das Warten.
    let addr = spawn_silent_server().await;

    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c.cancel();
    });

    let started = Instant::now();
    let err = run(&provider(addr), cancel)
        .await
        .expect_err("Abbruch beim Warten auf die Antwort muss durchschlagen");
    assert!(
        matches!(err, SeppError::Aborted),
        "Abbruch meldet sich als Aborted, war: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "der Abbruch wirkt sofort: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn huge_error_body_is_capped() {
    // Ein Anbieter, der auf einen abgelehnten Request eine Megabyte-Seite antwortet, darf weder
    // den Speicher noch (über die Fehlermeldung) das Kontextfenster fluten.
    let (addr, _bodies) = spawn_server(vec![Reply::new(400, "x".repeat(1_000_000))]).await;

    let err = run(&provider(addr), CancellationToken::new())
        .await
        .expect_err("400 muss durchschlagen");
    let msg = err.to_string();
    assert!(msg.contains("HTTP 400"), "{msg}");
    assert!(
        msg.len() < 70_000,
        "Body gedeckelt (64 KiB + Präfix), war {} Bytes",
        msg.len()
    );
    assert!(msg.len() > 60_000, "aber nicht leer: {} Bytes", msg.len());
}

/// Derselbe Wiederanlauf über den geteilten OpenAI-Pfad — er bedient auch Moonshot und z.ai.
#[cfg(feature = "moonshot")]
#[tokio::test]
async fn shared_openai_path_retries_too() {
    use sepp_provider::MoonshotProvider;

    let openai_sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (addr, bodies) = spawn_server(vec![
        Reply::new(429, "slow down").retry_after("0"),
        Reply::new(200, openai_sse),
    ])
    .await;

    let p = MoonshotProvider::new(Some("test-key".into()), format!("http://{addr}/v1"));
    let m = Model {
        id: "kimi-k3".into(),
        provider: "moonshot".into(),
        display_name: "Kimi K3".into(),
        context_window: 1_048_576,
        max_output_tokens: 32_768,
        supports_reasoning: false,
        supports_images: false,
    };
    let req = CompletionRequest {
        model: &m,
        system: None,
        messages: &[],
        tools: &[],
        thinking: ThinkingLevel::Off,
        max_tokens: 1024,
    };
    let events: Vec<StreamEvent> = p
        .stream(req, CancellationToken::new())
        .await
        .expect("der zweite Versuch muss durchkommen")
        .collect()
        .await;

    match events.first() {
        Some(StreamEvent::Notice { text }) => assert!(text.contains("moonshot"), "{text}"),
        other => panic!("erstes Ereignis muss ein Notice sein, war: {other:?}"),
    }
    assert_eq!(
        bodies.lock().expect("lock").len(),
        2,
        "genau ein Wiederanlauf"
    );
}
