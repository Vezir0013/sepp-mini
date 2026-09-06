//! HTTP über den Host (Feature `net`) — **provisorisch**.
//!
//! Die Typen hier entsprechen den Records `http-request`/`http-response` in `wit/sepp.wit`. Der
//! Host antwortet heute noch mit einem Fehler („noch nicht implementiert"); Stufe 3 macht
//! `host_http` zum durchsetzenden Proxy (Host-Allowlist exakt, Secrets an der Grenze, Audit je
//! Request). Die Kodierung binärer Bodies ist offen (heute UTF-8-Text) und wird additiv ergänzt.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::{__abi, ffi};

/// Eine Anfrage. Header sind Paare, wie im WIT `list<tuple<string, string>>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// Die Antwort des Hosts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub status: u16,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: String,
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

    /// Schickt eine fertige Anfrage über den Host.
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
    /// Fügt einen Header hinzu.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.req.headers.push((name.into(), value.into()));
        self
    }

    /// Setzt den Body (Text).
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.req.body = Some(body.into());
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
        // Ohne Body fehlt das Feld ganz — wie `option<list<u8>>` im WIT.
        let v = serde_json::to_value(Http::new().get("u").build()).unwrap();
        assert!(v.get("body").is_none(), "{v}");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_stub_explains_itself() {
        let e = Http::new().get("https://example.com").send().err().unwrap();
        assert!(e.message().contains("wasm32"), "{e}");
    }
}
