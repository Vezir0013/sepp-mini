//! `sepp-tools` — der `Tool`-Trait, Helfer (Truncation, File-Mutation-Queue) und die
//! eingebauten Tools (`read`/`write`/`edit`/`bash`).
//!
//! **Sepp Guard:** Jedes eingebaute Tool kann einen [`Guard`] tragen. Dann wird jede Aktion vor
//! dem I/O autorisiert (`read`/`write`/`edit`: Pfadprüfung; `bash`: Rückfrage-Muster und
//! Kindprozess in der OS-Sandbox mit der Agent-Policy). Ohne Guard (Tests, `--mode yolo`)
//! verhalten sich die Tools wie bisher.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use sepp_core::{Result, ToolResult, ToolSpec};
use sepp_policy::{Action, Actor, Authorization, Guard};

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

// Das Parameter-Schema entsteht in `sepp-core` (Feature `schema`), damit die eingebauten Tools
// und das Plugin-SDK dieselbe Bereinigung teilen. Der Pfad `sepp_tools::schema_for` bleibt.
pub use sepp_core::schema_for;

/// Die eingebauten Tools als gemeinsames Toolset — **ohne** Guard (wie `--mode yolo`).
/// Produktiv [`builtin_tools_with`] mit dem Guard des Agenten verwenden.
pub fn builtin_tools() -> Vec<Arc<dyn Tool>> {
    builtin_tools_with(None)
}

/// Die eingebauten Tools, alle an denselben Guard gebunden (`None` = ungesichert).
pub fn builtin_tools_with(guard: Option<Arc<Guard>>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadTool::new(guard.clone())),
        Arc::new(WriteTool::new(guard.clone())),
        Arc::new(EditTool::new(guard.clone())),
        Arc::new(BashTool::new(guard)),
    ]
}

/// Ergebnis einer Autorisierung beim Guard: die (evtl. leere) Zusatz-Gewährung für diesen
/// Aufruf und der Audit-Eintrag als JSON für `ToolResult.details["guard"]`.
pub(crate) struct Authorized {
    pub auth: Authorization,
    pub audit: Option<Value>,
}

/// Autorisiert eine Agent-Aktion, falls ein Guard gesetzt ist. Ohne Guard: immer erlaubt.
/// Bei Verweigerung kommt `SeppError::CapabilityDenied` mit Hinweis zum Freigeben zurück; der
/// Agent-Loop macht daraus ein Fehler-ToolResult fürs Modell.
pub(crate) async fn authorize(guard: Option<&Arc<Guard>>, action: Action) -> Result<Authorized> {
    match guard {
        None => Ok(Authorized {
            auth: Authorization::default(),
            audit: None,
        }),
        Some(g) => {
            // Das Ereignis kommt aus der `Authorization` dieses Aufrufs, nicht aus dem geteilten
            // Guard-Audit — Tool-Calls laufen parallel und würden sich sonst gegenseitig den
            // Eintrag wegnehmen.
            let auth = g.authorize(&Actor::Agent, action).await?;
            let audit = auth.audit.as_ref().map(Guard::audit_json);
            Ok(Authorized { auth, audit })
        }
    }
}

/// Hängt den Audit-Eintrag an `details["guard"]` (Details gehen nicht ans Modell).
pub(crate) fn with_guard_details(mut result: ToolResult, audit: Option<Value>) -> ToolResult {
    if let Some(a) = audit {
        if !result.details.is_object() {
            result.details = json!({});
        }
        if let Some(obj) = result.details.as_object_mut() {
            obj.insert(sepp_core::GUARD_DETAIL_KEY.into(), a);
        }
    }
    result
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;
    use std::sync::Arc;

    use sepp_policy::{
        AgentSection, BuiltinDefaults, Grants, Guard, Mode, NullSandbox, PolicyFile, PolicySet,
        ResolveCtx, Source,
    };

    /// Guard im Modus `mode`, der genau `dir` lesen und schreiben darf (kanonisiert), mit
    /// `NullSandbox` (keine OS-Durchsetzung — die Tests prüfen die Pfadprüfung).
    pub(crate) fn guard_for(dir: &Path, mode: Mode) -> Arc<Guard> {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let file = PolicyFile {
            mode: Some(mode),
            agent: Some(AgentSection {
                grants: Grants {
                    fs_read: vec![dir.display().to_string()],
                    fs_write: vec![dir.display().to_string()],
                    ..Grants::default()
                },
                ..AgentSection::default()
            }),
            ..PolicyFile::default()
        };
        let ctx = ResolveCtx {
            home: None,
            cwd: dir.clone(),
            tmpdir: dir,
        };
        let set = PolicySet::merge(
            vec![(Source::File("/test-policy.toml".into()), file)],
            &BuiltinDefaults::default(),
            None,
            &ctx,
        );
        Arc::new(Guard::new(set, Box::new(NullSandbox)))
    }
}
