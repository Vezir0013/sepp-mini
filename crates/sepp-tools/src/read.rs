//! `read` — Datei lesen (mit optionalem Zeilen-Offset/Limit), `truncate_head`.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use sepp_core::{Result, SeppError, ToolResult, ToolSpec};
use sepp_policy::{Action, Guard};

use crate::truncate::{truncate_head, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use crate::util::normalize_path;
use crate::{authorize, schema_for, with_guard_details, Tool};

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

/// Liest den Inhalt einer Datei. Mit Guard wird der Pfad vor dem Lesen autorisiert.
#[derive(Default)]
pub struct ReadTool {
    guard: Option<Arc<Guard>>,
}

impl ReadTool {
    pub fn new(guard: Option<Arc<Guard>>) -> Self {
        ReadTool { guard }
    }
}

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
        let authorized = authorize(self.guard.as_ref(), Action::FsRead(path.clone())).await?;

        let data = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| SeppError::Tool(format!("read {}: {e}", path.display())))?;

        // Byte-Bereich statt `lines().join("\n")`: sonst würde CRLF zu LF normalisiert und eine
        // abschließende Newline verschluckt — das Modell kopierte den `old_string` dann aus einer
        // Ausgabe, die es so auf der Platte nicht gibt, und `edit` fände null Treffer.
        let body = if p.offset.is_some() || p.limit.is_some() {
            crate::truncate::line_slice(&data, p.offset.unwrap_or(0), p.limit)
        } else {
            &data
        };

        let t = truncate_head(body, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let mut content = t.content.clone();
        if let Some(note) = t.note() {
            content.push_str(&note);
        }
        Ok(with_guard_details(
            ToolResult::text(content),
            authorized.audit,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::guard_for;
    use sepp_policy::Mode;
    use serde_json::json;

    #[tokio::test]
    async fn read_allowed_inside_grant_and_denied_outside() {
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(inside.path().join("a.txt"), "hallo").unwrap();
        std::fs::write(outside.path().join("b.txt"), "geheim").unwrap();
        let tool = ReadTool::new(Some(guard_for(inside.path(), Mode::Auto)));

        let ok = tool
            .execute(
                json!({ "path": inside.path().join("a.txt") }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(
            matches!(&ok.content[0], sepp_core::ContentBlock::Text { text } if text == "hallo")
        );
        assert_eq!(ok.details["guard"]["decision"], "allow");

        let err = tool
            .execute(
                json!({ "path": outside.path().join("b.txt") }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("verweigert"), "{msg}");
        assert!(msg.contains("sepp policy allow agent fs_read"), "{msg}");
    }

    #[tokio::test]
    async fn offset_limit_keeps_crlf_and_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("win.txt");
        std::fs::write(&p, "eins\r\nzwei\r\ndrei\r\n").unwrap();

        let r = ReadTool::default()
            .execute(
                json!({ "path": &p, "offset": 1, "limit": 1 }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        // Byte-exakt: sonst kopiert das Modell "zwei" ohne \r und `edit` findet nichts.
        assert!(
            matches!(&r.content[0], sepp_core::ContentBlock::Text { text } if text == "zwei\r\n"),
            "{:?}",
            r.content[0]
        );
    }

    #[tokio::test]
    async fn read_without_guard_keeps_legacy_behavior() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let r = ReadTool::default()
            .execute(
                json!({ "path": dir.path().join("a.txt") }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(r.details.is_null());
    }
}
