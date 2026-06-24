//! `edit` — exakter Textersatz (`old_string` → `new_string`) über die File-Mutation-Queue.

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
struct EditParams {
    /// Pfad der zu ändernden Datei.
    path: String,
    /// Exakt zu ersetzender Text. Muss eindeutig sein (außer `replace_all`).
    old_string: String,
    /// Ersetzungstext.
    new_string: String,
    /// Alle Vorkommen ersetzen statt genau eines.
    #[serde(default)]
    replace_all: bool,
}

/// Ersetzt exakten Text in einer Datei.
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            label: "Edit".into(),
            description: "Ersetzt exakten Text in einer Datei. `old_string` muss eindeutig \
                          vorkommen (sonst Fehler), es sei denn `replace_all` ist gesetzt."
                .into(),
            parameters: schema_for::<EditParams>(),
        }
    }

    async fn execute(
        &self,
        input: Value,
        _cancel: CancellationToken,
        _on_update: Option<&(dyn Fn(ToolResult) + Send + Sync)>,
    ) -> Result<ToolResult> {
        let p: EditParams = serde_json::from_value(input)
            .map_err(|e| SeppError::Tool(format!("edit: ungültige Parameter: {e}")))?;
        if p.old_string.is_empty() {
            return Err(SeppError::Tool(
                "edit: old_string darf nicht leer sein".into(),
            ));
        }
        let path = normalize_path(&p.path);
        let path_disp = path.display().to_string();

        let key_path = path.clone();
        let count = with_file_mutation_queue(&key_path, async move {
            let data = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| SeppError::Tool(format!("edit {}: {e}", path.display())))?;

            let count = data.matches(&p.old_string).count();
            if count == 0 {
                return Err(SeppError::Tool(format!(
                    "edit: old_string nicht gefunden in {}",
                    path.display()
                )));
            }
            if !p.replace_all && count > 1 {
                return Err(SeppError::Tool(format!(
                    "edit: old_string {count}x gefunden; mehr Kontext angeben oder replace_all=true"
                )));
            }

            let new_data = if p.replace_all {
                data.replace(&p.old_string, &p.new_string)
            } else {
                data.replacen(&p.old_string, &p.new_string, 1)
            };
            tokio::fs::write(&path, new_data.as_bytes())
                .await
                .map_err(|e| SeppError::Tool(format!("edit write {}: {e}", path.display())))?;
            Ok(count)
        })
        .await?;

        Ok(
            ToolResult::text(format!("{path_disp}: {count} Ersetzung(en)."))
                .with_details(json!({ "path": path_disp, "replacements": count })),
        )
    }
}
