//! Modell-Metadaten und Reasoning-Stufe.

use serde::{Deserialize, Serialize};

/// Beschreibt ein konkretes LLM-Modell eines Anbieters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub provider: String,
    pub display_name: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub supports_reasoning: bool,
    pub supports_images: bool,
}

/// Reasoning-/Thinking-Stufe für einen Completion-Request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
}

impl ThinkingLevel {
    /// Grober Thinking-Token-Budget-Vorschlag (0 = aus). Provider-Adapter können
    /// das auf ihr jeweiliges Feld abbilden.
    pub fn budget_tokens(self) -> u64 {
        match self {
            ThinkingLevel::Off => 0,
            ThinkingLevel::Minimal => 1024,
            ThinkingLevel::Low => 2048,
            ThinkingLevel::Medium => 4096,
            ThinkingLevel::High => 8192,
            ThinkingLevel::XHigh => 16384,
        }
    }
}
