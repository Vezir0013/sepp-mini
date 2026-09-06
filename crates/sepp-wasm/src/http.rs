//! Der HTTP-Worker hinter `host_http` — sepp ist der Netzwerkstack des Moduls.
//!
//! Ein Plugin hat keine Sockets; alles, was es ins Netz schickt, geht durch diesen Worker. Die
//! **Regeln** (Allowlist, Secrets, Zähler, Audit) prüft der Aufrufer in `lib.rs`, bevor ein
//! Auftrag hier ankommt; dieses Modul führt nur noch aus — mit den Deckeln, die zur Ausführung
//! gehören: Timeout, Abbruch, Größe der Antwort.
//!
//! **Warum ein eigener Thread mit eigener Runtime.** Die Host-Funktion läuft synchron in der
//! wasmi-Closure. Die liegt bei einem Werkzeugaufruf in `spawn_blocking`, beim Laden aber auf dem
//! Reactor-Thread der `current_thread`-Runtime der CLI — und ein Modul darf `host_http` auch aus
//! seiner Start-Sektion rufen. `Handle::block_on` würde dort panicken, `oneshot::blocking_recv`
//! ebenso. Deshalb: ein Thread `sepp-http`, der eine eigene Runtime treibt, Aufträge über einen
//! Kanal entgegennimmt und die Antwort über einen `std`-Kanal zurückgibt — beides blockiert
//! nirgends unerlaubt und hängt an keiner fremden Runtime. Der Thread startet beim ersten Auftrag
//! und endet, wenn der letzte [`HttpProxy`] fällt (der Sender geht mit ihm).
//!
//! **Keine Redirects.** Ein 3xx auf einen anderen Host wäre eine Anfrage, die die Allowlist nie
//! gesehen hat. Der Client folgt deshalb keinem Redirect; die 3xx-Antwort geht ans Plugin, das
//! selbst neu anfragen darf — jeder Hop läuft so durch dieselbe Prüfung samt Audit-Zeile, und ein
//! Secret-Header wandert nie automatisch an ein Redirect-Ziel.
//!
//! **Proxy-Umgebung.** `HTTPS_PROXY`/`NO_PROXY` gelten wie beim Provider-Client (reqwest-Default):
//! Die Variablen gehören der Umgebung des Nutzers, und die Allowlist bewertet den Ziel-Host in
//! der URL, den auch ein CONNECT-Proxy nur weiterreicht.

use std::fmt;
use std::sync::mpsc as std_mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;
use reqwest::Method;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_util::sync::CancellationToken;

/// Ein fertig geprüfter Auftrag: Header sind aufgelöst (Secret-Werte `sensitive`), der Host ist
/// gewährt, die Deckel stehen.
pub(crate) struct HttpJob {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    /// Zeitbudget für die ganze Anfrage inklusive Body — vom Aufrufer bereits auf die
    /// Rest-Wanduhr des Werkzeugaufrufs gekappt.
    pub timeout: Duration,
    /// Größte Antwort, die gepuffert wird; wird vor dem Lesen (Content-Length) und beim Streamen
    /// geprüft.
    pub max_response_bytes: u64,
    pub cancel: CancellationToken,
}

/// Die Antwort, wie sie der Aufrufer ans Modul weiterreicht.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpReply {
    pub status: u16,
    /// Header-Namen klein, Werte `from_utf8_lossy`.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Dauer der Anfrage in Millisekunden (für die Audit-Spur).
    pub ms: u64,
}

/// Warum keine Antwort kam. Wird zum `{"error":…}` fürs Modul und in die Audit-Spur — vom
/// Aufrufer noch durch den Broker redigiert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HttpFail {
    Timeout(Duration),
    Cancelled,
    /// Content-Length oder gestreamte Bytes über `max_response_bytes`.
    TooLarge {
        limit: u64,
    },
    /// Verbindung, TLS, Protokoll — der Text von reqwest.
    Transport(String),
    /// Der Worker selbst (Runtime, Thread, Client) ließ sich nicht bauen oder ist weg.
    Worker(String),
}

impl fmt::Display for HttpFail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpFail::Timeout(d) => write!(f, "keine Antwort innerhalb von {} ms", d.as_millis()),
            HttpFail::Cancelled => f.write_str("abgebrochen"),
            HttpFail::TooLarge { limit } => write!(
                f,
                "Antwort größer als {limit} Bytes (limits.max_http_response_bytes)"
            ),
            HttpFail::Transport(e) => write!(f, "Verbindung: {e}"),
            HttpFail::Worker(e) => write!(f, "HTTP-Worker: {e}"),
        }
    }
}

type Reply = Result<HttpReply, HttpFail>;
type Job = (HttpJob, std_mpsc::Sender<Reply>);

/// Der Zugang zum Worker. Ein Exemplar je `WasmHost`, per `Arc` in jedes Plugin gereicht; der
/// Thread startet beim ersten [`fetch`](HttpProxy::fetch).
pub(crate) struct HttpProxy {
    worker: OnceLock<Result<tokio_mpsc::UnboundedSender<Job>, String>>,
}

impl HttpProxy {
    pub(crate) fn new() -> Self {
        HttpProxy {
            worker: OnceLock::new(),
        }
    }

    /// Führt den Auftrag aus und wartet blockierend auf die Antwort. Von jedem Thread aus
    /// erlaubt — auch von einem, der selbst eine Runtime treibt.
    pub(crate) fn fetch(&self, job: HttpJob) -> Reply {
        let tx = match self.worker.get_or_init(start_worker) {
            Ok(tx) => tx,
            Err(e) => return Err(HttpFail::Worker(e.clone())),
        };
        let (reply_tx, reply_rx) = std_mpsc::channel();
        if tx.send((job, reply_tx)).is_err() {
            return Err(HttpFail::Worker("Thread ist beendet".into()));
        }
        reply_rx
            .recv()
            .unwrap_or_else(|_| Err(HttpFail::Worker("keine Antwort vom Thread".into())))
    }
}

/// Startet den Thread `sepp-http`. Die Runtime wird noch im aufrufenden Thread gebaut (das ist
/// überall erlaubt — nur `block_on` nicht), damit ein Fehler hier landet und nicht im Thread.
fn start_worker() -> Result<tokio_mpsc::UnboundedSender<Job>, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Runtime: {e}"))?;
    let (tx, rx) = tokio_mpsc::unbounded_channel::<Job>();
    std::thread::Builder::new()
        .name("sepp-http".into())
        .spawn(move || rt.block_on(serve(rx)))
        .map_err(|e| format!("Thread: {e}"))?;
    Ok(tx)
}

/// Die Schleife des Workers: Client einmal bauen, jeden Auftrag als eigene Task bedienen —
/// parallele Werkzeugaufrufe warten so nicht aufeinander. Endet, wenn der letzte Sender fällt.
async fn serve(mut rx: tokio_mpsc::UnboundedReceiver<Job>) {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .user_agent(format!("sepp/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("Client: {e}"));
    while let Some((job, reply)) = rx.recv().await {
        match &client {
            Ok(client) => {
                let client = client.clone();
                tokio::spawn(async move {
                    // Der Empfänger kann weg sein (Aufrufer abgebrochen) — dann verfällt die Antwort.
                    let _ = reply.send(perform(client, job).await);
                });
            }
            Err(e) => {
                let _ = reply.send(Err(HttpFail::Worker(e.clone())));
            }
        }
    }
}

/// Ein Auftrag: Abbruch geht vor, dann das Zeitbudget, dann die Anfrage selbst.
async fn perform(client: reqwest::Client, job: HttpJob) -> Reply {
    let HttpJob {
        method,
        url,
        headers,
        body,
        timeout,
        max_response_bytes,
        cancel,
    } = job;
    let started = Instant::now();
    let request = send_and_read(&client, method, &url, headers, body, max_response_bytes);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(HttpFail::Cancelled),
        r = tokio::time::timeout(timeout, request) => match r {
            Ok(Ok((status, headers, body))) => Ok(HttpReply {
                status,
                headers,
                body,
                ms: started.elapsed().as_millis() as u64,
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(HttpFail::Timeout(timeout)),
        },
    }
}

type Raw = (u16, Vec<(String, String)>, Vec<u8>);

/// Sendet und liest die Antwort gestreamt — der Deckel greift **vor** dem Puffern (Content-Length)
/// und beim Lesen, nicht erst auf dem fertigen Puffer (dasselbe Muster wie `read_granted_file`).
async fn send_and_read(
    client: &reqwest::Client,
    method: Method,
    url: &str,
    headers: HeaderMap,
    body: Vec<u8>,
    max: u64,
) -> Result<Raw, HttpFail> {
    let mut resp = client
        .request(method, url)
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|e| HttpFail::Transport(e.to_string()))?;
    if resp.content_length().is_some_and(|len| len > max) {
        return Err(HttpFail::TooLarge { limit: max });
    }
    let status = resp.status().as_u16();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| HttpFail::Transport(e.to_string()))?
    {
        if (buf.len() + chunk.len()) as u64 > max {
            return Err(HttpFail::TooLarge { limit: max });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok((status, headers, buf))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    use super::*;

    /// Ein Server, der genau eine Verbindung annimmt und sie dem Handler gibt. Liest vorher den
    /// Request-Kopf, damit der Client seine Anfrage los ist. Mit Lese-Timeout und festem Ende,
    /// sonst hinge ein Test bei einem Fehler ewig.
    fn server(handler: impl FnOnce(&mut TcpStream) + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while sock.read(&mut b).map(|n| n == 1).unwrap_or(false) {
                    head.push(b[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                handler(&mut sock);
            }
        });
        format!("http://{addr}")
    }

    fn respond(status: &'static str, extra: &'static str, body: &'static [u8]) -> String {
        server(move |sock| {
            let _ = write!(
                sock,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(body);
        })
    }

    fn job(url: &str) -> HttpJob {
        HttpJob {
            method: Method::GET,
            url: url.to_string(),
            headers: HeaderMap::new(),
            body: Vec::new(),
            timeout: Duration::from_secs(5),
            max_response_bytes: 1024,
            cancel: CancellationToken::new(),
        }
    }

    #[test]
    fn worker_starts_lazily_and_serves_sequential_requests() {
        let proxy = HttpProxy::new();
        assert!(proxy.worker.get().is_none(), "kein Thread ohne Auftrag");
        for _ in 0..2 {
            let url = respond("200 OK", "X-Test: ja\r\n", b"hallo");
            let reply = proxy.fetch(job(&format!("{url}/x"))).unwrap();
            assert_eq!(reply.status, 200);
            assert_eq!(reply.body, b"hallo");
            assert!(reply
                .headers
                .iter()
                .any(|(k, v)| k == "x-test" && v == "ja"));
        }
    }

    #[test]
    fn content_length_over_the_cap_is_refused_before_reading() {
        let url = server(|sock| {
            // Kündigt 10 KiB an, schickt aber nichts — würde gelesen, hinge der Test am Timeout.
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10240\r\n\r\n");
            std::thread::sleep(Duration::from_millis(300));
        });
        let started = Instant::now();
        let e = HttpProxy::new().fetch(job(&url)).unwrap_err();
        assert_eq!(e, HttpFail::TooLarge { limit: 1024 });
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "wurde vor dem Lesen abgelehnt"
        );
    }

    #[test]
    fn an_eof_delimited_body_over_the_cap_is_refused_while_streaming() {
        let url = server(|sock| {
            // Ohne Content-Length: der Deckel greift beim Streamen, nicht auf dem fertigen Puffer.
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
            let _ = sock.write_all(&[b'x'; 8192]);
        });
        let e = HttpProxy::new().fetch(job(&url)).unwrap_err();
        assert_eq!(e, HttpFail::TooLarge { limit: 1024 });
    }

    #[test]
    fn a_silent_server_hits_the_timeout() {
        let url = server(|_| std::thread::sleep(Duration::from_secs(3)));
        let mut j = job(&url);
        j.timeout = Duration::from_millis(200);
        let started = Instant::now();
        let e = HttpProxy::new().fetch(j).unwrap_err();
        assert_eq!(e, HttpFail::Timeout(Duration::from_millis(200)));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn cancel_ends_a_hanging_request() {
        let url = server(|_| std::thread::sleep(Duration::from_secs(3)));
        let j = job(&url);
        let token = j.cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            token.cancel();
        });
        let started = Instant::now();
        let e = HttpProxy::new().fetch(j).unwrap_err();
        assert_eq!(e, HttpFail::Cancelled);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_302_is_returned_not_followed() {
        let url = respond("302 Found", "Location: http://evil.example/\r\n", b"");
        let reply = HttpProxy::new().fetch(job(&url)).unwrap();
        assert_eq!(reply.status, 302);
        assert!(reply
            .headers
            .iter()
            .any(|(k, v)| k == "location" && v == "http://evil.example/"));
    }

    #[test]
    fn a_refused_connection_is_a_transport_error_not_a_panic() {
        // Port aus einem gebundenen und sofort wieder freigegebenen Listener — dort hört niemand.
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let e = HttpProxy::new()
            .fetch(job(&format!("http://{addr}/")))
            .unwrap_err();
        assert!(matches!(e, HttpFail::Transport(_)), "{e}");
    }
}
