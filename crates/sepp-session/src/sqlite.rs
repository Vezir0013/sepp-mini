//! Optionales SQLite-Session-Backend (Feature `sqlite`). Gleiche Baum-Semantik wie der
//! JSONL-Store (aktiver Pfad root→leaf, Compaction, Labels, Branch ephemer), aber in einer
//! eingebetteten SQLite-Datei (`<id>.sqlite`, WAL). `rusqlite` mit `bundled` → statisch, keine
//! System-Abhängigkeit.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use rusqlite::{params, Connection, OptionalExtension};

use sepp_core::{Message, Result, SeppError};

use crate::{
    now_millis, Entry, EntryId, EntryPayload, SessionInfo, SessionStore, Tree, FORMAT_VERSION,
};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS entries (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    id         TEXT NOT NULL UNIQUE,
    parent_id  TEXT,
    timestamp  INTEGER NOT NULL,
    label      TEXT,
    payload    TEXT NOT NULL
);";

fn err(e: impl std::fmt::Display) -> SeppError {
    SeppError::Session(format!("sqlite: {e}"))
}

fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT v FROM meta WHERE k = ?1", [key], |r| {
        r.get::<_, String>(0)
    })
    .optional()
    .map_err(err)
}

/// SQLite-Store: jede Mutation committet sofort (WAL, `synchronous=NORMAL`); `flush()` macht den
/// Checkpoint. Durabilitätsmodell wie JSONL (flush() ist die Grenze).
pub struct SqliteSessionStore {
    tree: Tree,
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl std::fmt::Debug for SqliteSessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteSessionStore")
            .field("id", &self.tree.id)
            .field("path", &self.path)
            .finish()
    }
}

impl SqliteSessionStore {
    fn open_conn(path: &Path) -> Result<Connection> {
        let conn = Connection::open(path).map_err(err)?;
        // WAL + synchronous=NORMAL (empfohlene WAL-Einstellung): Commits liegen im durablen WAL
        // und werden beim Öffnen wiedergespielt → kein Datenverlust bei App-/Prozess-Crash; nur
        // bei Strom-/Kernel-Crash können die letzten ungeflushten Commits verloren gehen — das
        // gleiche Durabilitätsmodell wie der JSONL-Store (flush() ist die Grenze). FULL würde
        // pro Append fsyncen und damit den Single-Thread-Reactor blockieren.
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
            .map_err(err)?;
        Ok(conn)
    }

    /// Legt eine neue Session in `project_dir` an (`<id>.sqlite`).
    pub fn create(project_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(project_dir)?;
        let id = uuid::Uuid::new_v4().to_string();
        let path = project_dir.join(format!("{id}.sqlite"));
        let conn = Self::open_conn(&path)?;
        conn.execute_batch(SCHEMA).map_err(err)?;
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let created_at = now_millis();
        let meta = [
            ("version", FORMAT_VERSION.to_string()),
            ("session_id", id.clone()),
            ("cwd", cwd),
            ("created_at", created_at.to_string()),
        ];
        for (k, v) in meta {
            conn.execute("INSERT INTO meta (k, v) VALUES (?1, ?2)", params![k, v])
                .map_err(err)?;
        }
        Ok(SqliteSessionStore {
            tree: Tree::new(id),
            conn: Mutex::new(conn),
            path,
        })
    }

    /// Öffnet eine vorhandene `.sqlite`-Session.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Self::open_conn(path)?;
        let session_id = meta_get(&conn, "session_id")?
            .ok_or_else(|| SeppError::Session("sqlite: Session ohne session_id".into()))?;

        let mut entries: Vec<Entry> = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT id, parent_id, timestamp, label, payload FROM entries ORDER BY seq",
                )
                .map_err(err)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })
                .map_err(err)?;
            for row in rows {
                let (id, parent_id, timestamp, label, payload_s) = row.map_err(err)?;
                let payload: EntryPayload = serde_json::from_str(&payload_s)
                    .map_err(|e| SeppError::Session(format!("sqlite payload: {e}")))?;
                entries.push(Entry {
                    id,
                    parent_id,
                    timestamp,
                    label,
                    payload,
                });
            }
        }

        let mut index = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            index.insert(e.id.clone(), i);
        }
        // Aktiver Leaf = zuletzt angehängter Eintrag (wie JSONL; Branch ist ephemer).
        let leaf = entries.last().map(|e| e.id.clone());

        Ok(SqliteSessionStore {
            tree: Tree {
                id: session_id,
                entries,
                index,
                leaf,
            },
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        })
    }

    /// Öffnet die zuletzt geänderte `.sqlite`-Session in `project_dir`.
    pub fn continue_recent(project_dir: &Path) -> Result<Self> {
        let newest = Self::list(project_dir)?
            .into_iter()
            .max_by_key(|s| s.modified)
            .ok_or_else(|| SeppError::Session("keine vorhandene SQLite-Session gefunden".into()))?;
        Self::open(&newest.path)
    }

    /// Listet alle `.sqlite`-Sessions in `project_dir`.
    pub fn list(project_dir: &Path) -> Result<Vec<SessionInfo>> {
        let mut out = Vec::new();
        let rd = match std::fs::read_dir(project_dir) {
            Ok(rd) => rd,
            Err(_) => return Ok(out),
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sqlite") {
                continue; // WAL/SHM-Sidecars (`.sqlite-wal`) haben eine andere Extension
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if let Some(info) = read_info(&path, modified) {
                out.push(info);
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.modified));
        Ok(out)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn read_info(path: &Path, modified: i64) -> Option<SessionInfo> {
    let conn = Connection::open(path).ok()?;
    let id = meta_get(&conn, "session_id").ok().flatten()?;
    let created_at = meta_get(&conn, "created_at")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let name = meta_get(&conn, "name").ok().flatten();
    let entry_count: usize = conn
        .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get::<_, i64>(0))
        .map(|n| n as usize)
        .unwrap_or(0);
    Some(SessionInfo {
        id,
        path: path.to_path_buf(),
        created_at,
        modified,
        name,
        entry_count,
    })
}

#[async_trait::async_trait]
impl SessionStore for SqliteSessionStore {
    fn id(&self) -> &str {
        &self.tree.id
    }
    fn path_messages(&self) -> Vec<Message> {
        self.tree.path_messages()
    }
    fn append(&mut self, payload: EntryPayload) -> Result<EntryId> {
        let entry = self.tree.append_entry(payload);
        let payload_s = serde_json::to_string(&entry.payload)
            .map_err(|e| SeppError::Session(format!("sqlite payload: {e}")))?;
        self.locked()
            .execute(
                "INSERT INTO entries (id, parent_id, timestamp, label, payload) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    entry.id,
                    entry.parent_id,
                    entry.timestamp,
                    entry.label,
                    payload_s
                ],
            )
            .map_err(err)?;
        Ok(entry.id)
    }
    fn branch(&mut self, to: &EntryId) -> Result<()> {
        self.tree.branch(to) // ephemer (nicht persistiert, wie JSONL)
    }
    fn set_label(&mut self, id: &EntryId, label: Option<String>) -> Result<()> {
        self.tree.set_label(id, label.clone())?;
        self.locked()
            .execute(
                "UPDATE entries SET label = ?1 WHERE id = ?2",
                params![label, id],
            )
            .map_err(err)?;
        Ok(())
    }
    fn entries(&self) -> &[Entry] {
        &self.tree.entries
    }
    fn leaf(&self) -> Option<&EntryId> {
        self.tree.leaf.as_ref()
    }
    async fn flush(&mut self) -> Result<()> {
        // Jede Mutation committet bereits (WAL); ein Checkpoint genügt für Durabilität.
        self.locked()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sepp_core::ContentBlock;

    fn user(text: &str) -> EntryPayload {
        EntryPayload::Message {
            message: Message::user_text(text),
        }
    }

    #[tokio::test]
    async fn sqlite_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let (id, path);
        {
            let mut s = SqliteSessionStore::create(dir.path()).unwrap();
            s.append(user("hallo")).unwrap();
            s.append(user("welt")).unwrap();
            s.flush().await.unwrap();
            id = s.id().to_string();
            path = s.path().to_path_buf();
        }
        let s = SqliteSessionStore::open(&path).unwrap();
        assert_eq!(s.id(), id);
        assert_eq!(s.path_messages().len(), 2);
        assert_eq!(SqliteSessionStore::list(dir.path()).unwrap().len(), 1);
        assert_eq!(
            SqliteSessionStore::continue_recent(dir.path())
                .unwrap()
                .id(),
            id
        );
    }

    #[tokio::test]
    async fn sqlite_labels_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let (path, eid);
        {
            let mut s = SqliteSessionStore::create(dir.path()).unwrap();
            eid = s.append(user("x")).unwrap();
            s.set_label(&eid, Some("checkpoint".into())).unwrap();
            s.flush().await.unwrap();
            path = s.path().to_path_buf();
        }
        let s = SqliteSessionStore::open(&path).unwrap();
        let labeled = s.entries().iter().find(|e| e.id == eid).unwrap();
        assert_eq!(labeled.label.as_deref(), Some("checkpoint"));
    }

    #[tokio::test]
    async fn sqlite_empty_dir_lists_nothing_and_continue_errs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(SqliteSessionStore::list(dir.path()).unwrap().is_empty());
        assert!(SqliteSessionStore::continue_recent(dir.path()).is_err());
    }

    #[tokio::test]
    async fn sqlite_tolerates_corrupt_db_file() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.sqlite");
        std::fs::write(&bad, b"das ist keine datenbank").unwrap();
        // list() überspringt die kaputte Datei (kein Panic), open() meldet einen Fehler.
        assert!(SqliteSessionStore::list(dir.path()).unwrap().is_empty());
        assert!(SqliteSessionStore::open(&bad).is_err());
    }

    #[tokio::test]
    async fn sqlite_compaction_and_branch() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = SqliteSessionStore::create(dir.path()).unwrap();
        let a = s.append(user("alt 1")).unwrap();
        let cut = s.append(user("alt 2")).unwrap();
        s.append(EntryPayload::Compaction {
            summary: "Z".into(),
            replaced_until: cut,
        })
        .unwrap();
        s.append(user("neu")).unwrap();
        let msgs = s.path_messages();
        assert_eq!(msgs.len(), 2); // [summary, "neu"]
        assert!(matches!(&msgs[0].content[0],
            ContentBlock::Text { text } if text.contains("Z")));

        // Branch (ephemer) auf einen früheren Eintrag.
        s.branch(&a).unwrap();
        s.append(user("anderer ast")).unwrap();
        assert_eq!(s.path_messages().len(), 2); // a -> "anderer ast"
        assert_eq!(s.entries().len(), 5); // nichts gelöscht
    }
}
