//! `sepp-hooks` — Tier-1-Hooks: leichte, synchrone Eingriffe in den Agent-Loop per Skript.
//!
//! Engine: **Rhai** (pure Rust, leicht zu gaten). Skripte definieren Handler-Funktionen
//! (`on_before_agent_start`, `on_input`, `on_tool_call`, `on_tool_result`, `on_turn_end`) und
//! nutzen die vom Host bereitgestellte, **gegatete** API (`block`, `continue_`, `handled`,
//! `notify`, `log`) — KEIN ungehinderter Datei-/Netzzugriff (Policy-Durchsetzung: Phase 4).

mod rhai_host;

use serde_json::Value;

use sepp_core::{Message, Result, ToolResult};

pub use rhai_host::RhaiHookHost;

/// Ereignis an einem Hook-Punkt. Mutable Felder erlauben Transformation.
#[derive(Debug)]
pub enum HookEvent<'a> {
    BeforeAgentStart {
        system_prompt: &'a mut String,
    },
    Input {
        text: &'a mut String,
    },
    ToolCall {
        name: &'a str,
        input: &'a mut Value,
    },
    ToolResult {
        name: &'a str,
        result: &'a mut ToolResult,
    },
    TurnEnd {
        message: &'a Message,
    },
}

/// Ergebnis eines Hooks (steuert den Loop).
#[derive(Debug, Clone)]
pub enum HookOutcome {
    /// Normal weiter.
    Continue,
    /// Tool-Call blocken (nur für `ToolCall` sinnvoll).
    Block { reason: String },
    /// Eingabe direkt behandelt, ohne LLM (nur für `Input` sinnvoll).
    Handled,
}

/// Quergriff in den Agent-Loop an definierten Punkten.
pub trait HookHost: Send + Sync {
    fn dispatch(&self, event: HookEvent<'_>) -> Result<HookOutcome>;
}
