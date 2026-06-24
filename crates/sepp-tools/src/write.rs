//! `write` — Datei schreiben/anlegen (über die File-Mutation-Queue, legt Verzeichnisse an).

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use sepp_core::{Result, SeppError, ToolResult, ToolSpec};

use crate::file_queue::with_file_mutation_queue;
use crate::util::normalize_path;
use crate::{schema_for, Tool};

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteParams {
    /// Zielpfad. Nicht existierende Elternverzeichnisse werden angelegt.
    path: String,
    /// Vollständiger neuer Dateiinhalt.
    content: String,
}

/// Schreibt (oder überschreibt) eine Datei.
pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            label: "Write".into(),
            description: "Schreibt `content` vollständig in `path` (überschreibt vorhandene \
                          Dateien). Fehlende Verzeichnisse werden angelegt."
                .into(),
            parameters: schema_for::<WriteParams>(),
        }
    }

    async fn execute(
        &self,
        input: Value,
        _cancel: CancellationToken,
        _on_update: Option<&(dyn Fn(ToolResult) + Send + Sync)>,
    ) -> Result<ToolResult> {
        let p: WriteParams = serde_json::from_value(input)
            .map_err(|e| SeppError::Tool(format!("write: ungültige Parameter: {e}")))?;
        let path = normalize_path(&p.path);
        let path_disp = path.display().to_string();
        let bytes = p.content.len();

        let key_path = path.clone();
        let content = p.content;
        with_file_mutation_queue(&key_path, async move {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| {
                        SeppError::Tool(format!("write: Verzeichnis {}: {e}", parent.display()))
                    })?;
                }
            }
            tokio::fs::write(&path, content.as_bytes())
                .await
                .map_err(|e| SeppError::Tool(format!("write {}: {e}", path.display())))?;
            Ok(())
        })
        .await?;

        Ok(
            ToolResult::text(format!("{path_disp} geschrieben ({bytes} Bytes)."))
                .with_details(json!({ "path": path_disp, "bytes": bytes })),
        )
    }
}
