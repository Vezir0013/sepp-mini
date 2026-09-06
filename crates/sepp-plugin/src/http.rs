//! HTTP über den Host (Feature `net`) — sepp ist der Netzwerkstack des Moduls.
//!
//! Ein Plugin hat keine Sockets; jede Anfrage geht durch `host_http`, und der Host setzt an der
//! Grenze durch, was ein Plugin selbst gar nicht könnte:
//!
//! - **Host-Allowlist je Anfrage.** Der Host der URL muss dem Plugin gewährt sein (Manifest
//!   `net = ["api.example.com"]` **und** `policy.toml [plugin.<name>] net = [...]`; Muster
//!   `*.example.com` oder `*`). Sonst geht kein Byte auf die Leitung, und der Fehler nennt den
//!   passenden `sepp policy allow`-Befehl.
//! - **Secrets, die das Modul nie sieht.** Ein Header-Wert darf `$NAME` enthalten; der Host
//!   ersetzt es — nur wenn der Host gewährt ist **und** die Variable per `env = ["NAME"]` in
//!   Manifest und Policy gewährt **und** in der Umgebung gesetzt ist. In der URL sind
//!   Platzhalter verboten (die URL steht in jeder Fehlermeldung).
//! - **Keine automatischen Redirects.** Ein 3xx kommt als [`Response`] zurück; wer folgen will,
//!   fragt selbst neu an — und läuft damit wieder durch die Allowlist.
//! - **Deckel** aus dem Manifest `[limits]`: `max_http_requests` je Werkzeugaufruf,
//!   `max_http_response_bytes`, `http_timeout_ms` je Anfrage (auf die Rest-Wanduhr gekappt).
//! - **Audit.** Jede Anfrage, auch eine abgelehnte, steht in der Spur der Sitzung (`sepp audit`)
//!   — mit den Namen ersetzter Secrets, nie mit Werten.
//!
//! Bodies: Text bleibt Text. Eine Antwort, die kein gültiges UTF-8 ist, kommt base64-kodiert
//! an; [`Response::bytes`] und [`Response::text`] kümmern sich darum. Für eine binäre Anfrage
//! gibt es [`RequestBuilder::body_bytes`]. Die Typen hier entsprechen den Records
//! `http-request`/`http-response` in `wit/sepp.wit`.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::{__abi, ffi};

/// Eine Anfrage. Header sind Paare, wie im WIT `list<tuple<string, string>>`; genau eines von
/// `body` (Text) und `body_base64` (Bytes) darf gesetzt sein.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_base64: Option<String>,
}

/// Die Antwort des Hosts. `body` ist gesetzt, wenn die Antwort gültiges UTF-8 war, sonst
/// `body_base64`; [`Response::bytes`] und [`Response::text`] verbergen den Unterschied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub status: u16,
    /// Header-Namen kleingeschrieben, wie der Host sie liefert.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub body_base64: Option<String>,
    /// Rohe Länge des Bodys in Bytes.
    #[serde(default)]
    pub bytes: u64,
}

impl Response {
    /// Der Body als Bytes — dekodiert base64, wenn die Antwort binär war.
    pub fn bytes(&self) -> Result<Vec<u8>> {
        match &self.body_base64 {
            Some(b64) => base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| Error::new(format!("body_base64: {e}"))),
            None => Ok(self.body.as_bytes().to_vec()),
        }
    }

    /// Der Body als Text — strikt UTF-8; eine binäre Antwort, die kein Text ist, ist ein Fehler.
    pub fn text(&self) -> Result<String> {
        match &self.body_base64 {
            Some(_) => Ok(String::from_utf8(self.bytes()?)?),
            None => Ok(self.body.clone()),
        }
    }

    /// Der erste Header dieses Namens (Groß-/Kleinschreibung egal).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `200..=299`?
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Der HTTP-Zugang des Hosts. Über [`crate::Host::http`].
#[derive(Debug, Clone, Copy)]
pub struct Http {
    _priv: (),
}

impl Http {
    pub(crate) fn new() -> Self {
        Http { _priv: () }
    }

    /// Beginnt eine `GET`-Anfrage.
    pub fn get(&self, url: impl Into<String>) -> RequestBuilder {
        self.request("GET", url)
    }

    /// Beginnt eine `POST`-Anfrage.
    pub fn post(&self, url: impl Into<String>) -> RequestBuilder {
        self.request("POST", url)
    }

    /// Beginnt eine Anfrage mit beliebiger Methode.
    pub fn request(&self, method: impl Into<String>, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder {
            req: Request {
                method: method.into(),
                url: url.into(),
                ..Request::default()
            },
        }
    }

    /// Schickt eine fertige Anfrage über den Host. Ein `Err` kommt vom Host mit Erklärung —
    /// nicht gewährter Host, fehlendes Secret, Timeout, zu große Antwort — oder von der
    /// Kodierung. Ein HTTP-Fehlerstatus ist **kein** `Err`: siehe [`Response::is_success`].
    pub fn send(&self, req: &Request) -> Result<Response> {
        let input = serde_json::to_vec(req)?;
        let n = ffi::http(&input)?;
        __abi::decode_json(n, ffi::fetch)
    }
}

/// Baut eine [`Request`] Schritt für Schritt.
#[derive(Debug, Clone)]
pub struct RequestBuilder {
    req: Request,
}

impl RequestBuilder {
    /// Fügt einen Header hinzu. Der Wert darf `$NAME` enthalten — der Host setzt das Secret ein.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.req.headers.push((name.into(), value.into()));
        self
    }

    /// Setzt den Body als Text (ersetzt einen zuvor gesetzten Byte-Body).
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.req.body = Some(body.into());
        self.req.body_base64 = None;
        self
    }

    /// Setzt den Body als Bytes (ersetzt einen zuvor gesetzten Text-Body).
    pub fn body_bytes(mut self, body: impl AsRef<[u8]>) -> Self {
        self.req.body_base64 = Some(base64::engine::general_purpose::STANDARD.encode(body));
        self.req.body = None;
        self
    }

    /// Die fertige Anfrage, ohne sie zu senden.
    pub fn build(self) -> Request {
        self.req
    }

    /// Sendet die Anfrage über den Host.
    pub fn send(self) -> Result<Response> {
        Http::new().send(&self.req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_matches_the_wit_record_shape() {
        let req = Http::new()
            .get("https://example.com/x")
            .header("Accept", "application/json")
            .body("{}")
            .build();
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["method"], "GET");
        assert_eq!(
            v["headers"],
            serde_json::json!([["Accept", "application/json"]])
        );
        assert_eq!(v["body"], "{}");
        assert!(v.get("body_base64").is_none(), "{v}");
        // Ohne Body fehlt das Feld ganz — wie `option<list<u8>>` im WIT.
        let v = serde_json::to_value(Http::new().get("u").build()).unwrap();
        assert!(v.get("body").is_none(), "{v}");
    }

    #[test]
    fn body_bytes_serializes_as_base64_and_replaces_a_text_body() {
        let req = Http::new()
            .post("https://example.com/up")
            .body("weg damit")
            .body_bytes([0u8, 255, 254])
            .build();
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["body_base64"], "AP/+");
        assert!(v.get("body").is_none(), "{v}");
        // Und zurück: Text ersetzt Bytes.
        let v = serde_json::to_value(Http::new().post("u").body_bytes([1u8]).body("text").build())
            .unwrap();
        assert_eq!(v["body"], "text");
        assert!(v.get("body_base64").is_none());
    }

    #[test]
    fn response_decodes_text_or_base64_and_finds_headers() {
        let text: Response = serde_json::from_str(
            r#"{"status":200,"headers":[["content-type","text/plain"]],"body":"hallo","bytes":5}"#,
        )
        .unwrap();
        assert_eq!(text.bytes().unwrap(), b"hallo");
        assert_eq!(text.text().unwrap(), "hallo");
        assert_eq!(text.header("Content-Type"), Some("text/plain"));
        assert_eq!(text.header("x-fehlt"), None);
        assert!(text.is_success());

        let bin: Response = serde_json::from_str(
            r#"{"status":404,"headers":[],"body_base64":"AP/+","bytes":3,"extra":"toleriert"}"#,
        )
        .unwrap();
        assert_eq!(bin.bytes().unwrap(), vec![0, 255, 254]);
        assert!(bin.text().is_err(), "kein UTF-8 → kein Text");
        assert!(!bin.is_success());

        let bad: Response =
            serde_json::from_str(r#"{"status":200,"body_base64":"nicht base64!"}"#).unwrap();
        assert!(bad.bytes().is_err());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_stub_explains_itself() {
        let e = Http::new().get("https://example.com").send().err().unwrap();
        assert!(e.message().contains("wasm32"), "{e}");
    }
}
