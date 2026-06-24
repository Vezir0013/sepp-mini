//! `sepp-core` — Kerntypen und reine Logik für sepp mini.
//!
//! Diese Crate hat bewusst **minimale Abhängigkeiten** (serde, thiserror) und macht
//! **kein I/O** und kein async. Alle höheren Crates bauen auf diesen Typen auf.

pub mod error;
pub mod message;
pub mod model;
pub mod tool;

pub use error::{Result, SeppError};
pub use message::{ContentBlock, ImageSource, Message, Role, Usage};
pub use model::{Model, ThinkingLevel};
pub use tool::{ToolResult, ToolSpec};
