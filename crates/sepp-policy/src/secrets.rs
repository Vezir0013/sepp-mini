//! Minimaler Secret-Broker.
//!
//! Erweiterungen sollen Secrets **nie im Klartext sehen**: sie nutzen Platzhalter `$NAME`, der
//! Broker ersetzt sie durch echte Werte **nur** für erlaubte Hosts (Policy `Net{host}`). Keys
//! kommen aus Env-Vars, werden **nie persistiert**; [`redact`](SecretBroker::redact) maskiert
//! Secret-Werte fürs Logging.

use std::collections::HashMap;

use crate::guard::Actor;
use crate::{Capability, Policy};

/// Hält Secrets im Speicher und ersetzt/maskiert Platzhalter.
#[derive(Debug, Default, Clone)]
pub struct SecretBroker {
    secrets: HashMap<String, String>,
}

/// Warum ein Platzhalter **nicht** ersetzt werden darf — das Doppel-Gate in drei Antworten:
/// wohin (Net), welches Secret (Env), und ist es überhaupt gesetzt.
///
/// Der MCP-Client (Secret-Header) und der WASM-Host (`host_http`) prüfen dieselbe Kette über
/// [`SecretBroker::gate`] und erklären das Ergebnis über [`GateRefusal::explain`] — so nennen
/// beide denselben `sepp policy allow`-Befehl, und keiner nennt je den Wert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateRefusal {
    /// Der Ziel-Host ist dem Akteur nicht gewährt.
    NetNotGranted { host: String },
    /// Die Variable ist dem Akteur nicht gewährt.
    EnvNotGranted,
    /// Gewährt, aber in der Umgebung nicht (oder leer) gesetzt.
    EnvNotSet,
}

impl GateRefusal {
    /// Der Satz für die Meldung: beginnt mit `nutzt $NAME`, nennt den passenden Befehl und
    /// **nie** einen Wert. Den Kontext („Header 'authorization' ") stellt der Aufrufer voran.
    pub fn explain(&self, actor: &Actor, placeholder: &str) -> String {
        let who = actor.cli_name();
        match self {
            GateRefusal::NetNotGranted { host } => format!(
                "nutzt ${placeholder}, aber {host} ist nicht gewährt — \
                 `sepp policy allow {who} net {host}`"
            ),
            GateRefusal::EnvNotGranted => format!(
                "nutzt ${placeholder}, aber diese Variable ist nicht gewährt — \
                 `sepp policy allow {who} env {placeholder}`"
            ),
            GateRefusal::EnvNotSet => {
                format!("nutzt ${placeholder} — gewährt, aber in der Umgebung nicht gesetzt")
            }
        }
    }
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

    /// Lädt aus der Umgebung **genau** die Platzhalter aus `texts`, die die Policy per `Env`
    /// gewährt — nicht mehr. Der einzige Weg, auf dem Secret-Werte aus der Umgebung in einen
    /// Broker kommen sollten: Ohne das Env-Gate entschiede allein die Konfiguration (die nach
    /// Trust auch projektlokal sein kann), welche Variable rausgeht, und `[deny]` kann `env`
    /// nicht einschränken.
    pub fn from_env_for<'a>(texts: impl IntoIterator<Item = &'a str>, policy: &Policy) -> Self {
        let wanted: Vec<&str> = texts
            .into_iter()
            .flat_map(placeholder_names)
            .filter(|n| {
                policy.allows(&Capability::Env {
                    name: (*n).to_string(),
                })
            })
            .collect();
        SecretBroker::from_env(&wanted)
    }

    /// Das Doppel-Gate für einen Platzhalter, in dieser Reihenfolge: Ist `host` gewährt? Ist
    /// die Variable gewährt? Ist sie gesetzt? Der erste Verstoß gewinnt — Fail-closed, bevor ein
    /// Byte die Leitung sieht.
    pub fn gate(&self, name: &str, host: &str, policy: &Policy) -> Result<(), GateRefusal> {
        if !policy.allows(&Capability::Net {
            host: host.to_string(),
        }) {
            return Err(GateRefusal::NetNotGranted {
                host: host.to_string(),
            });
        }
        if !policy.allows(&Capability::Env {
            name: name.to_string(),
        }) {
            return Err(GateRefusal::EnvNotGranted);
        }
        if !self.knows(name) {
            return Err(GateRefusal::EnvNotSet);
        }
        Ok(())
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
    fn gate_checks_net_then_env_then_presence() {
        let b = SecretBroker::new().with_secret("TOKEN", "sk-geheim");
        let both = Policy::new(vec![
            Capability::Net {
                host: "api.example.com".into(),
            },
            Capability::Env {
                name: "TOKEN".into(),
            },
        ]);
        assert_eq!(b.gate("TOKEN", "api.example.com", &both), Ok(()));
        assert_eq!(
            b.gate("TOKEN", "evil.example", &both),
            Err(GateRefusal::NetNotGranted {
                host: "evil.example".into()
            })
        );
        // Net offen, Env nicht.
        assert_eq!(
            b.gate("TOKEN", "api.example.com", &net("api.example.com")),
            Err(GateRefusal::EnvNotGranted)
        );
        // Beides gewährt, aber der Broker kennt den Wert nicht (Umgebung leer).
        assert_eq!(
            SecretBroker::new().gate("TOKEN", "api.example.com", &both),
            Err(GateRefusal::EnvNotSet)
        );
    }

    #[test]
    fn explain_names_the_command_for_the_actor_and_never_a_value() {
        let mcp = Actor::Mcp("gh".into());
        let plugin = Actor::Plugin("datev".into());
        let net = GateRefusal::NetNotGranted {
            host: "api.example.com".into(),
        };
        let m = net.explain(&mcp, "TOKEN");
        assert!(m.contains("nutzt $TOKEN"), "{m}");
        assert!(
            m.contains("`sepp policy allow mcp.gh net api.example.com`"),
            "{m}"
        );
        let p = GateRefusal::EnvNotGranted.explain(&plugin, "TOKEN");
        assert!(
            p.contains("`sepp policy allow plugin.datev env TOKEN`"),
            "{p}"
        );
        let s = GateRefusal::EnvNotSet.explain(&plugin, "TOKEN");
        assert!(s.contains("nicht gesetzt"), "{s}");
        for text in [&m, &p, &s] {
            assert!(!text.contains("sk-"), "{text}");
        }
    }

    #[test]
    fn from_env_for_loads_only_granted_and_used_variables() {
        std::env::set_var("SEPP_TEST_SECRETS_A", "a-wert");
        std::env::set_var("SEPP_TEST_SECRETS_B", "b-wert");
        std::env::set_var("SEPP_TEST_SECRETS_C", "c-wert");
        let policy = Policy::new(vec![
            Capability::Env {
                name: "SEPP_TEST_SECRETS_A".into(),
            },
            Capability::Env {
                name: "SEPP_TEST_SECRETS_C".into(),
            },
        ]);
        // A verlangt und gewährt → geladen; B verlangt, nicht gewährt → nicht; C gewährt, aber
        // von keinem Text verlangt → ebenfalls nicht (nur, was wirklich gebraucht wird).
        let b = SecretBroker::from_env_for(
            ["Bearer $SEPP_TEST_SECRETS_A", "x-$SEPP_TEST_SECRETS_B"],
            &policy,
        );
        assert!(b.knows("SEPP_TEST_SECRETS_A"));
        assert!(!b.knows("SEPP_TEST_SECRETS_B"));
        assert!(!b.knows("SEPP_TEST_SECRETS_C"));
    }

    #[test]
    fn redact_hides_values() {
        let b = SecretBroker::new().with_secret("K", "supersecret");
        assert_eq!(b.redact("token=supersecret end"), "token=[REDACTED] end");
        assert!(!b.redact("supersecret").contains("supersecret"));
    }
}
