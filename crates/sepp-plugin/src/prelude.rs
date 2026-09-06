//! Alles, was ein Plugin üblicherweise braucht: `use sepp_plugin::prelude::*;`.
//!
//! [`Result`] ist das SDK-Ergebnis (`Result<T, sepp_plugin::Error>`); wer den Fehlertyp selbst
//! wählen will, schreibt `Result<T, MeinFehler>`.

pub use crate::{tool, Error, Host, Result};

pub use sepp_core::{ContentBlock, ToolResult, ToolSpec};

pub use schemars::JsonSchema;
pub use serde::Deserialize;
pub use serde_json::{json, Value};

#[cfg(feature = "net")]
pub use crate::http::{Http, Request, RequestBuilder, Response};
#[cfg(feature = "fs-read")]
pub use crate::Fs;
