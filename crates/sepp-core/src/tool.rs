//! Tool-Datentypen (der `Tool`-Trait selbst lebt in `sepp-tools`, da er async ist).

use serde::{Deserialize, Serialize};

use crate::message::ContentBlock;

/// Ergebnis eines Tool-Aufrufs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Geht ans Modell.
    pub content: Vec<ContentBlock>,
    /// Strukturierte Daten fürs Rendering/State — NICHT ans Modell.
    #[serde(default)]
    pub details: serde_json::Value,
    #[serde(default)]
    pub is_error: bool,
}

impl ToolResult {
    /// Erfolgreiches Text-Ergebnis.
    pub fn text(text: impl Into<String>) -> Self {
        ToolResult {
            content: vec![ContentBlock::text(text)],
            details: serde_json::Value::Null,
            is_error: false,
        }
    }

    /// Fügt strukturierte `details` hinzu (Builder-Stil).
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }
}

/// Beschreibung eines Tools fürs LLM (Name, Doku, JSON-Schema der Parameter).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub label: String,
    pub description: String,
    /// JSON-Schema der Parameter (z. B. via `schemars` erzeugt).
    pub parameters: serde_json::Value,
}

/// Obergrenze für Tool-Namen — die engste Grenze der unterstützten Anbieter.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Erfüllt der Name die Anbieter-Regel `^[A-Za-z0-9_-]{1,64}$`?
///
/// Anthropic und OpenAI lehnen alles andere mit `400` ab — und zwar den **ganzen** Request, nicht
/// nur das eine Werkzeug. Ein einziger Doppelpunkt aus einer fremden Quelle (MCP-Server,
/// WASM-Plugin) legt damit jeden Turn lahm, bis der Server abgeklemmt wird. Deshalb wird der Name
/// geprüft, bevor er ins Toolset wandert.
pub fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TOOL_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Macht einen Namen regelkonform: unerlaubte Bytes werden zu `_`, die Länge wird gekappt.
///
/// Für Quellen, deren Namen der Nutzer **nicht** in der Hand hat (MCP-Server). Dort wäre
/// Ablehnen die schlechtere Wahl: Das Werkzeug verschwände kommentarlos. Der Aufrufer behält den
/// Originalnamen für den entfernten Aufruf; angepasst wird nur, was das Modell zu sehen bekommt.
/// Namen, die bereits gültig sind, bleiben unverändert.
pub fn sanitize_tool_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    out.truncate(MAX_TOOL_NAME_LEN);
    if out.is_empty() {
        out.push_str("tool");
    }
    out
}

/// Erzeugt ein bereinigtes JSON-Schema für einen Parameter-Typ: `$schema` und `title` werden
/// entfernt, weil Anbieter wie Anthropic ein schlankes `input_schema` erwarten.
///
/// Die eingebauten Tools (`sepp-tools`) und das Plugin-SDK (`sepp-plugin`) erzeugen ihr Schema
/// hierüber — eine Stelle, damit beide Wege dasselbe Ergebnis liefern.
#[cfg(feature = "schema")]
pub fn schema_for<T: schemars::JsonSchema>() -> serde_json::Value {
    let mut v = serde_json::to_value(schemars::schema_for!(T))
        .unwrap_or_else(|_| serde_json::json!({ "type": "object" }));
    if let Some(obj) = v.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names_are_accepted() {
        for n in ["read", "git__status", "a", "A-1_b", &"x".repeat(64)] {
            assert!(is_valid_tool_name(n), "{n}");
        }
    }

    #[test]
    fn invalid_names_are_rejected() {
        for n in [
            "",
            "rp:pdf_extract",
            "mit leerzeichen",
            "grüße",
            "a.b",
            &"x".repeat(65),
        ] {
            assert!(!is_valid_tool_name(n), "{n}");
        }
    }

    /// Doc-Kommentare an den Feldern werden zu `description`; `$schema` und `title` fehlen,
    /// weil Anbieter ein schlankes `input_schema` erwarten.
    #[cfg(feature = "schema")]
    #[test]
    fn schema_for_is_a_bare_object_schema() {
        #[derive(schemars::JsonSchema)]
        #[allow(dead_code)]
        struct Params {
            /// Der Pfad.
            path: String,
            limit: Option<usize>,
        }
        let v = schema_for::<Params>();
        assert_eq!(v["type"], "object");
        assert_eq!(v["required"], serde_json::json!(["path"]));
        assert_eq!(v["properties"]["path"]["description"], "Der Pfad.");
        assert!(v.get("$schema").is_none(), "{v}");
        assert!(v.get("title").is_none(), "{v}");
    }

    #[test]
    fn sanitize_fixes_and_leaves_valid_names_alone() {
        assert_eq!(sanitize_tool_name("rp:pdf_extract"), "rp_pdf_extract");
        assert_eq!(sanitize_tool_name("git__status"), "git__status");
        assert_eq!(sanitize_tool_name("mit leerzeichen"), "mit_leerzeichen");
        // Nicht-ASCII wird je `char` ersetzt, nicht je Byte — sonst zerfiele ein Umlaut in zwei.
        assert_eq!(sanitize_tool_name("grüße"), "gr__e");
        assert_eq!(sanitize_tool_name(""), "tool");
        assert_eq!(sanitize_tool_name(&"x".repeat(100)).len(), 64);
        // Das Ergebnis ist immer gültig — das ist die eigentliche Zusage.
        for n in ["rp:pdf_extract", "", "grüße", &"x".repeat(100), "@@@"] {
            assert!(is_valid_tool_name(&sanitize_tool_name(n)), "{n}");
        }
    }
}
