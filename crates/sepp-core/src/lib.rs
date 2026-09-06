//! `sepp-core` — Kerntypen und reine Logik für sepp mini.
//!
//! Diese Crate hat bewusst **minimale Abhängigkeiten** (serde, thiserror; optional schemars
//! hinter dem Feature `schema`) und macht **kein I/O** und kein async. Alle höheren Crates
//! bauen auf diesen Typen auf — auch das Guest-SDK `sepp-plugin`, das für `wasm32` kompiliert.

pub mod error;
pub mod message;
pub mod model;
pub mod tool;

pub use error::{Result, SeppError};
pub use message::{ContentBlock, ImageSource, Message, Role, Usage};
pub use model::{Model, ThinkingLevel};
#[cfg(feature = "schema")]
pub use tool::schema_for;
pub use tool::{is_valid_tool_name, sanitize_tool_name, ToolResult, ToolSpec, MAX_TOOL_NAME_LEN};
