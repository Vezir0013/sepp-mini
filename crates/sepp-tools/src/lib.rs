//! `sepp-tools` — der `Tool`-Trait, Helfer (Truncation, File-Mutation-Queue) und die
//! eingebauten Tools (`read`/`write`/`edit`/`bash`).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use sepp_core::{Result, ToolResult, ToolSpec};

mod bash;
mod edit;
pub mod file_queue;
mod read;
pub mod truncate;
mod util;
mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use file_queue::with_file_mutation_queue;
pub use read::ReadTool;
pub use truncate::{
    truncate_content_blocks, truncate_head, truncate_tail, Truncated, DEFAULT_MAX_BYTES,
    DEFAULT_MAX_LINES,
};
pub use write::WriteTool;

/// Ein aufrufbares Tool.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Beschreibung fürs LLM (Name, Doku, Parameter-Schema).
    fn spec(&self) -> ToolSpec;

    /// Führt das Tool aus. `on_update` erlaubt optionalen Streaming-Fortschritt;
    /// `cancel` muss respektiert werden.
    async fn execute(
        &self,
        input: Value,
        cancel: CancellationToken,
        on_update: Option<&(dyn Fn(ToolResult) + Send + Sync)>,
    ) -> Result<ToolResult>;
}

/// Erzeugt ein bereinigtes JSON-Schema für einen Parameter-Typ (`$schema`/`title` entfernt,
/// da Anbieter wie Anthropic ein schlankes `input_schema` erwarten).
pub fn schema_for<T: schemars::JsonSchema>() -> Value {
    let mut v = serde_json::to_value(schemars::schema_for!(T))
        .unwrap_or_else(|_| serde_json::json!({ "type": "object" }));
    if let Some(obj) = v.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
    }
    v
}

/// Die in Phase 1 eingebauten Tools als gemeinsames Toolset.
pub fn builtin_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadTool),
        Arc::new(WriteTool),
        Arc::new(EditTool),
        Arc::new(BashTool),
    ]
}
