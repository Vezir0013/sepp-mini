//! Minimaler Secret-Broker.
//!
//! Erweiterungen sollen Secrets **nie im Klartext sehen**: sie nutzen Platzhalter `$NAME`, der
//! Broker ersetzt sie durch echte Werte **nur** für erlaubte Hosts (Policy `Net{host}`). Keys
//! kommen aus Env-Vars, werden **nie persistiert**; [`redact`](SecretBroker::redact) maskiert
//! Secret-Werte fürs Logging.

use std::collections::HashMap;

use crate::{Capability, Policy};

/// Hält Secrets im Speicher und ersetzt/maskiert Platzhalter.
#[derive(Debug, Default, Clone)]
pub struct SecretBroker {
    secrets: HashMap<String, String>,
}

/// Findet die `$NAME`-Platzhalter in `text` und liefert je Fund `(start, ende, name)` —
/// `start` zeigt auf das `$`, `ende` hinter das letzte Namenszeichen.
///
/// **Der** Scanner: Wer wissen will, welche Secrets ein Text braucht, und wer sie ersetzt,
/// müssen sich exakt einig sein. Zwei Implementierungen driften, und eine Abweichung hieße
/// hier, dass ein Platzhalter am Gate vorbeikäme.
fn scan(text: &str) -> impl Iterator<Item = (usize, usize, &str)> {
    let bytes = text.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < bytes.len() {
            if bytes[i] == b'$' {
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                if j > start {
                    let found = (i, j, &text[start..j]);
                    i = j;
                    return Some(found);
                }
            }
            i += 1;
        }
        None
    })
}

/// Namen aller `$NAME`-Platzhalter in `text`, in Reihenfolge des Auftretens.
///
/// Für Aufrufer, die *vor* der Ersetzung wissen müssen, welche Secrets ein Text überhaupt
/// verlangt — etwa um nur die gewährten Env-Vars zu laden, statt die ganze Umgebung.
pub fn placeholder_names(text: &str) -> Vec<&str> {
    scan(text).map(|(_, _, name)| name).collect()
}

impl SecretBroker {
    /// Leerer Broker.
    pub fn new() -> Self {
        SecretBroker::default()
    }

    /// Fügt ein Secret hinzu (Builder-Stil).
    pub fn with_secret(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.secrets.insert(name.into(), value.into());
        self
    }

    /// Lädt die genannten Env-Vars als Secrets (fehlende werden übersprungen).
    pub fn from_env(names: &[&str]) -> Self {
        let mut b = SecretBroker::new();
        for n in names {
            if let Ok(v) = std::env::var(n) {
                if !v.is_empty() {
                    b.secrets.insert((*n).to_string(), v);
                }
            }
        }
        b
    }

    /// Sind keine Secrets hinterlegt?
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// Kennt der Broker ein Secret dieses Namens? Für Aufrufer, die einen fehlenden Wert
    /// **melden** wollen, statt stumm den Platzhalter stehen zu lassen.
    pub fn knows(&self, name: &str) -> bool {
        self.secrets.contains_key(name)
    }

    /// Ersetzt `$NAME`-Platzhalter durch echte Werte — **nur**, wenn die Policy `Net{host}`
    /// erlaubt. Für nicht erlaubte Hosts bleibt der Platzhalter stehen (kein Leak).
    pub fn substitute_for_host(&self, text: &str, host: &str, policy: &Policy) -> String {
        if !policy.allows(&Capability::Net {
            host: host.to_string(),
        }) {
            return text.to_string();
        }
        // Segment-basiert: literale Läufe werden als &str-Slices kopiert (UTF-8-sicher); nur
        // erkannte `$NAME` werden ersetzt. Geschnitten wird ausschließlich an `$`-Positionen
        // (ASCII) und an Namensgrenzen, nie mitten in einem Mehrbyte-Zeichen.
        let mut out = String::with_capacity(text.len());
        let mut last = 0; // Beginn des noch nicht geflushten Literal-Laufs
        for (start, end, name) in scan(text) {
            if let Some(v) = self.secrets.get(name) {
                out.push_str(&text[last..start]); // Literal vor dem '$'
                out.push_str(v);
                last = end;
            }
            // unbekannt → `$NAME` bleibt Teil des Literal-Laufs (verbatim)
        }
        out.push_str(&text[last..]);
        out
    }

    /// Maskiert vorkommende Secret-**Werte** durch `[REDACTED]` (fürs Logging).
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        // Längste Werte zuerst, damit Teilstrings nicht teilmaskiert werden.
        let mut values: Vec<&String> = self.secrets.values().filter(|v| !v.is_empty()).collect();
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        for v in values {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), "[REDACTED]");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(host: &str) -> Policy {
        Policy::new(vec![Capability::Net {
            host: host.to_string(),
        }])
    }

    #[test]
    fn substitutes_only_for_allowed_host() {
        let b = SecretBroker::new().with_secret("API_KEY", "sk-123");
        let text = "Authorization: Bearer $API_KEY";
        // erlaubter Host → ersetzt
        assert_eq!(
            b.substitute_for_host(text, "api.example.com", &net("api.example.com")),
            "Authorization: Bearer sk-123"
        );
        // nicht erlaubter Host → Platzhalter bleibt (kein Leak)
        assert_eq!(
            b.substitute_for_host(text, "evil.com", &net("api.example.com")),
            "Authorization: Bearer $API_KEY"
        );
    }

    #[test]
    fn unknown_placeholder_is_kept() {
        let b = SecretBroker::new().with_secret("A", "1");
        assert_eq!(
            b.substitute_for_host("$A and $B", "h", &net("h")),
            "1 and $B"
        );
    }

    #[test]
    fn substitution_is_utf8_safe() {
        let b = SecretBroker::new().with_secret("K", "v");
        // Nicht-ASCII rund um Platzhalter und im Literal-Lauf bleibt unversehrt.
        let text = "Grüße äöü — $K — 日本語 €";
        assert_eq!(
            b.substitute_for_host(text, "h", &net("h")),
            "Grüße äöü — v — 日本語 €"
        );
        // Unbekannter Platzhalter zwischen Nicht-ASCII bleibt verbatim.
        assert_eq!(
            b.substitute_for_host("café $UNKNOWN café", "h", &net("h")),
            "café $UNKNOWN café"
        );
    }

    #[test]
    fn placeholder_names_agree_with_substitution() {
        // `$` ohne Namen, führende Ziffer, Unterstrich, doppeltes `$` — der Scanner muss genau
        // das finden, was die Ersetzung später anfasst, sonst käme ein Platzhalter am Gate vorbei.
        let text = "$A$ $_1 $ $9x $LANG_2";
        assert_eq!(placeholder_names(text), vec!["A", "_1", "9x", "LANG_2"]);

        let b = SecretBroker::new()
            .with_secret("A", "a")
            .with_secret("_1", "u")
            .with_secret("9x", "n")
            .with_secret("LANG_2", "l");
        assert_eq!(b.substitute_for_host(text, "h", &net("h")), "a$ u $ n l");
        assert!(placeholder_names("gar nichts hier").is_empty());
    }

    #[test]
    fn redact_hides_values() {
        let b = SecretBroker::new().with_secret("K", "supersecret");
        assert_eq!(b.redact("token=supersecret end"), "token=[REDACTED] end");
        assert!(!b.redact("supersecret").contains("supersecret"));
    }
}
