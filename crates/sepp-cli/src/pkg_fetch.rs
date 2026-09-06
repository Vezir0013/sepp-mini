//! HTTP für `sepp pkg`: der [`Fetcher`] hinter `install <name>` und `search`.
//!
//! `sepp pkg` läuft ohne Tokio; dieser Fetcher bringt seine eigene `current_thread`-Runtime mit
//! (Muster `run()` in `main.rs`) — deshalb **nie** aus einem async-Kontext bauen, eine Runtime in
//! einer Runtime panickt. Gehärtet wie `host_http`: Verbindungs- und Lese-Timeout, User-Agent,
//! Deckel **vor** dem Puffern (Content-Length) und beim Streamen, Weiterleitungen nur bis
//! [`MAX_REDIRECTS`] und nur auf Ziele, die die Schema-Regel erfüllen (`https://`, `http://`
//! nur Loopback) — dieselbe Funktion wie überall in `sepp-pkg`. Proxy-Variablen der Umgebung
//! gelten, wie beim Provider-Client. Kein Guard, kein Audit: `sepp pkg` ist eine Handlung des
//! Nutzers wie `install.sh`.

use std::io::Write;
use std::time::Duration;

use anyhow::Context as _;

use sepp_core::{Result, SeppError};
use sepp_pkg::{check_url_scheme, Fetcher};

/// Höchstzahl an Weiterleitungen je Anfrage.
pub const MAX_REDIRECTS: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Inaktivität je Lesevorgang, kein Gesamtbudget — zusammen mit dem Deckel trotzdem endlich.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Ein blockierender HTTP-Fetcher auf eigener Runtime.
pub struct HttpFetcher {
    rt: tokio::runtime::Runtime,
    client: reqwest::Client,
}

impl HttpFetcher {
    /// Baut Runtime und Client. Nur aus synchronem Code rufen.
    pub fn new() -> anyhow::Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("Runtime für den Download")?;
        let client = reqwest::Client::builder()
            .redirect(redirect_policy())
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .user_agent(format!("sepp/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("HTTP-Client")?;
        Ok(HttpFetcher { rt, client })
    }
}

/// Folgt höchstens [`MAX_REDIRECTS`] Weiterleitungen, und nur auf Ziele nach der Schema-Regel.
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error(format!("mehr als {MAX_REDIRECTS} Weiterleitungen"));
        }
        match check_url_scheme(attempt.url().as_str()) {
            Ok(()) => attempt.follow(),
            Err(e) => attempt.error(format!("Weiterleitung abgelehnt: {e}")),
        }
    })
}

/// Fehlertext samt Ursachenkette — reqwest versteckt die Meldung einer Redirect-Policy in der
/// `source`.
fn describe(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        msg.push_str(&format!(": {s}"));
        src = s.source();
    }
    msg
}

impl Fetcher for HttpFetcher {
    fn fetch_to_writer(&self, url: &str, max: u64, out: &mut dyn Write) -> Result<u64> {
        check_url_scheme(url)?;
        self.rt.block_on(async {
            let mut resp = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|e| SeppError::Config(format!("pkg: {url}: {}", describe(&e))))?;
            let status = resp.status();
            if !status.is_success() {
                return Err(SeppError::Config(format!(
                    "pkg: {url}: HTTP {}",
                    status.as_u16()
                )));
            }
            if resp.content_length().is_some_and(|len| len > max) {
                return Err(SeppError::Config(format!(
                    "pkg: {url}: Antwort ist größer als {max} Bytes"
                )));
            }
            let mut total: u64 = 0;
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| SeppError::Config(format!("pkg: {url}: {}", describe(&e))))?
            {
                total += chunk.len() as u64;
                if total > max {
                    return Err(SeppError::Config(format!(
                        "pkg: {url}: Antwort ist größer als {max} Bytes"
                    )));
                }
                out.write_all(&chunk)
                    .map_err(|e| SeppError::Config(format!("pkg: {url}: {e}")))?;
            }
            Ok(total)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    struct Reply {
        status: &'static str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        /// Ohne `Content-Length` liest der Client bis zum Verbindungsende — so lässt sich der
        /// Deckel beim Streamen prüfen.
        with_length: bool,
    }

    fn ok(body: &[u8]) -> Reply {
        Reply {
            status: "200 OK",
            headers: vec![],
            body: body.to_vec(),
            with_length: true,
        }
    }

    fn redirect(to: &str) -> Reply {
        Reply {
            status: "302 Found",
            headers: vec![("Location".into(), to.into())],
            body: vec![],
            with_length: true,
        }
    }

    fn not_found() -> Reply {
        Reply {
            status: "404 Not Found",
            headers: vec![],
            body: b"nix".to_vec(),
            with_length: true,
        }
    }

    /// Server auf 127.0.0.1, der Verbindungen nacheinander bedient, bis der Prozess endet.
    /// `handler(pfad, kopf)` liefert die Antwort; immer `Connection: close`, sonst hielte
    /// reqwest die Verbindung offen und die nächste Anfrage hinge.
    fn serve(handler: impl Fn(&str, &str) -> Reply + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { break };
                sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while sock.read(&mut b).map(|n| n == 1).unwrap_or(false) {
                    head.push(b[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let r = handler(&path, &head);
                let mut resp = format!("HTTP/1.1 {}\r\nConnection: close\r\n", r.status);
                if r.with_length {
                    resp.push_str(&format!("Content-Length: {}\r\n", r.body.len()));
                }
                for (k, v) in &r.headers {
                    resp.push_str(&format!("{k}: {v}\r\n"));
                }
                resp.push_str("\r\n");
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.write_all(&r.body);
                let _ = sock.flush();
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn fetches_body_counts_bytes_and_sends_the_user_agent() {
        let base = serve(|path, head| match path {
            // hyper schreibt Header-Namen klein.
            "/a" if head.to_lowercase().contains("user-agent: sepp/") => ok(b"hallo"),
            "/a" => Reply {
                status: "500 Kein User-Agent",
                headers: vec![],
                body: vec![],
                with_length: true,
            },
            _ => not_found(),
        });
        let f = HttpFetcher::new().unwrap();
        assert_eq!(f.fetch(&format!("{base}/a"), 100).unwrap(), b"hallo");
        let mut buf = Vec::new();
        assert_eq!(
            f.fetch_to_writer(&format!("{base}/a"), 5, &mut buf)
                .unwrap(),
            5
        );
        assert_eq!(buf, b"hallo");
    }

    #[test]
    fn follows_loopback_redirects_then_stops_at_the_limit() {
        let base = serve(|path, _| match path {
            "/r0" => redirect("/r1"),
            "/r1" => redirect("/r2"),
            "/r2" => ok(b"ziel"),
            "/loop" => redirect("/loop"),
            _ => not_found(),
        });
        let f = HttpFetcher::new().unwrap();
        assert_eq!(f.fetch(&format!("{base}/r0"), 100).unwrap(), b"ziel");
        let e = f
            .fetch(&format!("{base}/loop"), 100)
            .unwrap_err()
            .to_string();
        assert!(e.contains("Weiterleitungen"), "{e}");
    }

    #[test]
    fn redirect_to_non_loopback_http_is_refused_without_connecting() {
        let base = serve(|path, _| match path {
            "/evil" => redirect("http://evil.example/x"),
            _ => not_found(),
        });
        let f = HttpFetcher::new().unwrap();
        let e = f
            .fetch(&format!("{base}/evil"), 100)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("Weiterleitung abgelehnt") && e.contains("https"),
            "{e}"
        );
    }

    #[test]
    fn content_length_over_the_cap_is_refused_before_reading() {
        let base = serve(|_, _| ok(&vec![b'x'; 5000]));
        let f = HttpFetcher::new().unwrap();
        let e = f
            .fetch(&format!("{base}/big"), 1000)
            .unwrap_err()
            .to_string();
        assert!(e.contains("größer als 1000"), "{e}");
        assert_eq!(f.fetch(&format!("{base}/big"), 5000).unwrap().len(), 5000);
    }

    #[test]
    fn body_over_the_cap_is_refused_while_streaming() {
        let base = serve(|_, _| Reply {
            status: "200 OK",
            headers: vec![],
            body: vec![b'y'; 5000],
            with_length: false,
        });
        let f = HttpFetcher::new().unwrap();
        let mut buf = Vec::new();
        let e = f
            .fetch_to_writer(&format!("{base}/stream"), 1000, &mut buf)
            .unwrap_err()
            .to_string();
        assert!(e.contains("größer als 1000"), "{e}");
        assert!(
            buf.len() <= 1000,
            "nichts über den Deckel hinaus geschrieben"
        );
    }

    #[test]
    fn a_404_is_an_error_with_the_status() {
        let base = serve(|_, _| not_found());
        let f = HttpFetcher::new().unwrap();
        let e = f.fetch(&format!("{base}/x"), 100).unwrap_err().to_string();
        assert!(e.contains("HTTP 404"), "{e}");
    }

    #[test]
    fn non_loopback_http_and_other_schemes_are_refused_before_any_request() {
        let f = HttpFetcher::new().unwrap();
        for bad in [
            "http://evil.example/x",
            "ftp://x/y",
            "pkg.example/index.toml",
        ] {
            let e = f.fetch(bad, 100).unwrap_err().to_string();
            assert!(e.contains("https"), "{bad}: {e}");
        }
    }
}
