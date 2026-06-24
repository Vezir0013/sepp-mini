//! `read` — Datei lesen (mit optionalem Zeilen-Offset/Limit), `truncate_head`.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use sepp_core::{Result, SeppError, ToolResult, ToolSpec};

use crate::truncate::{truncate_head, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use crate::util::normalize_path;
use crate::{schema_for, Tool};

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadParams {
    /// Pfad der zu lesenden Datei.
    path: String,
    /// Optionaler Start-Zeilen-Offset (0-basiert).
    #[serde(default)]
    offset: Option<usize>,
    /// Optionale maximale Zeilenzahl ab dem Offset.
    #[serde(default)]
    limit: Option<usize>,
}

/// Liest den Inhalt einer Datei.
pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            label: "Read".into(),
            description: "Liest eine Datei als Text. Optional ab `offset` (Zeile, 0-basiert) \
                          mit maximal `limit` Zeilen. Lange Ausgaben werden gekürzt."
                .into(),
            parameters: schema_for::<ReadParams>(),
        }
    }

    async fn execute(
        &self,
        input: Value,
        _cancel: CancellationToken,
        _on_update: Option<&(dyn Fn(ToolResult) + Send + Sync)>,
    ) -> Result<ToolResult> {
        let p: ReadParams = serde_json::from_value(input)
            .map_err(|e| SeppError::Tool(format!("read: ungültige Parameter: {e}")))?;
        let path = normalize_path(&p.path);

        let data = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| SeppError::Tool(format!("read {}: {e}", path.display())))?;

        let body = if p.offset.is_some() || p.limit.is_some() {
            let lines: Vec<&str> = data.lines().collect();
            let start = p.offset.unwrap_or(0).min(lines.len());
            let end = match p.limit {
                Some(l) => start.saturating_add(l).min(lines.len()),
                None => lines.len(),
            };
            lines[start..end].join("\n")
        } else {
            data
        };

        let t = truncate_head(&body, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let mut content = t.content.clone();
        if let Some(note) = t.note() {
            content.push_str(&note);
        }
        Ok(ToolResult::text(content))
    }
}
