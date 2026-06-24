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
