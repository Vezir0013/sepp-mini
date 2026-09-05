//! `write` — Datei schreiben/anlegen (über die File-Mutation-Queue, legt Verzeichnisse an).

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use sepp_core::{Result, SeppError, ToolResult, ToolSpec};
use sepp_policy::{Action, Guard};

use crate::file_queue::with_file_mutation_queue;
use crate::util::normalize_path;
use crate::{authorize, schema_for, with_guard_details, Tool};

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteParams {
    /// Zielpfad. Nicht existierende Elternverzeichnisse werden angelegt.
    path: String,
    /// Vollständiger neuer Dateiinhalt.
    content: String,
}

/// Schreibt (oder überschreibt) eine Datei. Mit Guard wird der Pfad vor dem Schreiben
/// autorisiert (kanonisch, auch für noch nicht existierende Dateien).
#[derive(Default)]
pub struct WriteTool {
    guard: Option<Arc<Guard>>,
}

impl WriteTool {
    pub fn new(guard: Option<Arc<Guard>>) -> Self {
        WriteTool { guard }
    }
}

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
        let authorized = authorize(self.guard.as_ref(), Action::FsWrite(path.clone())).await?;
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

        Ok(with_guard_details(
            ToolResult::text(format!("{path_disp} geschrieben ({bytes} Bytes)."))
                .with_details(json!({ "path": path_disp, "bytes": bytes })),
            authorized.audit,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::guard_for;
    use sepp_policy::Mode;

    #[tokio::test]
    async fn write_allowed_inside_grant_and_denied_outside() {
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let tool = WriteTool::new(Some(guard_for(inside.path(), Mode::Auto)));

        let ok = tool
            .execute(
                json!({ "path": inside.path().join("sub/new.txt"), "content": "x" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(inside.path().join("sub/new.txt").exists());
        assert_eq!(ok.details["bytes"], 1);
        assert_eq!(ok.details["guard"]["decision"], "allow");

        let err = tool
            .execute(
                json!({ "path": outside.path().join("x.txt"), "content": "x" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("sepp policy allow agent fs_write"));
        assert!(!outside.path().join("x.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_new_file_under_symlinked_parent_is_checked_canonically() {
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // Symlink IM Projekt zeigt nach draußen: der kanonische Zielpfad liegt außerhalb.
        let link = inside.path().join("escape");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let tool = WriteTool::new(Some(guard_for(inside.path(), Mode::Auto)));
        let err = tool
            .execute(
                json!({ "path": link.join("new.txt"), "content": "x" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("verweigert"), "{err}");
        assert!(!outside.path().join("new.txt").exists());
    }
}
