//! `sepp-session` — baumstrukturierte, persistente Sessions.
//!
//! Default-Backend ist JSONL (append-only, git-freundlich; Format: `specs/session-format.md`):
//! erste Zeile Header, danach `entry`/`label_change`-Zeilen. Der **aktive Pfad** (root→leaf)
//! ist die Conversation; `compaction`-Einträge ersetzen einen Pfad-Präfix durch eine
//! Zusammenfassung.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use sepp_core::{ContentBlock, Message, Result, Role, SeppError};

#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteSessionStore;

const FORMAT_VERSION: u32 = 1;

/// Identität eines Eintrags.
pub type EntryId = String;

/// Ein Knoten im Session-Baum.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    pub parent_id: Option<EntryId>,
    pub timestamp: i64,
    #[serde(default)]
    pub label: Option<String>,
    pub payload: EntryPayload,
}

/// Nutzlast eines Eintrags. Wire-Format exakt nach `specs/session-format.md`
/// (`Message` ist deshalb eine Struct-Variante mit `message`-Feld, nicht ein Newtype).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum EntryPayload {
    Message {
        message: Message,
    },
    Compaction {
        summary: String,
        replaced_until: EntryId,
    },
    Custom {
        kind: String,
        data: serde_json::Value,
    },
}

/// Header (erste Zeile der Datei).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Header {
    version: u32,
    session_id: String,
    cwd: String,
    created_at: i64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    parent_session: Option<String>,
}

/// Eine Zeile der JSONL-Datei.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Line {
    Header(Header),
    Entry(Entry),
    LabelChange { id: EntryId, label: Option<String> },
}

/// Kompakte Beschreibung einer Session-Datei (für Auswahl-Listen, `-r`).
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub path: PathBuf,
    pub created_at: i64,
    pub modified: i64,
    pub name: Option<String>,
    pub entry_count: usize,
}

/// Abstraktion über einen Session-Speicher.
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    fn id(&self) -> &str;
    /// Aktive Conversation: Pfad root→leaf, Compaction aufgelöst, nur Messages.
    fn path_messages(&self) -> Vec<Message>;
    fn append(&mut self, payload: EntryPayload) -> Result<EntryId>;
    fn branch(&mut self, to: &EntryId) -> Result<()>;
    fn set_label(&mut self, id: &EntryId, label: Option<String>) -> Result<()>;
    fn entries(&self) -> &[Entry];
    /// Aktueller Leaf (Ende des aktiven Pfads), z. B. zum Markieren in `/tree`.
    fn leaf(&self) -> Option<&EntryId>;
    /// Persistiert auf Platte (No-op bei In-Memory).
    async fn flush(&mut self) -> Result<()>;
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Synthetische Message für einen Compaction-Summary. Bewusst `User` (nicht System/Assistant
/// wie in der Spec angedeutet), damit die aufgelöste Conversation mit einer User-Message
/// beginnt (Anbieter wie Anthropic verlangen das); aufeinanderfolgende User-Messages werden
/// im Provider-Adapter zusammengeführt.
pub fn summary_message(summary: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::text(format!(
            "[Zusammenfassung des bisherigen Gesprächs]\n\n{summary}"
        ))],
        usage: None,
    }
}

/// Gemeinsame Baum-Logik beider Stores.
#[derive(Debug)]
struct Tree {
    id: String,
    entries: Vec<Entry>,
    index: HashMap<EntryId, usize>,
    leaf: Option<EntryId>,
}

impl Tree {
    fn new(id: String) -> Self {
        Tree {
            id,
            entries: Vec::new(),
            index: HashMap::new(),
            leaf: None,
        }
    }

    fn append_entry(&mut self, payload: EntryPayload) -> Entry {
        let entry = Entry {
            id: uuid::Uuid::new_v4().to_string(),
            parent_id: self.leaf.clone(),
            timestamp: now_millis(),
            label: None,
            payload,
        };
        self.index.insert(entry.id.clone(), self.entries.len());
        self.leaf = Some(entry.id.clone());
        self.entries.push(entry.clone());
        entry
    }

    fn branch(&mut self, to: &EntryId) -> Result<()> {
        if !self.index.contains_key(to) {
            return Err(SeppError::Session(format!(
                "branch: unbekannter Eintrag {to}"
            )));
        }
        self.leaf = Some(to.clone());
        Ok(())
    }

    fn set_label(&mut self, id: &EntryId, label: Option<String>) -> Result<()> {
        let idx = *self
            .index
            .get(id)
            .ok_or_else(|| SeppError::Session(format!("set_label: unbekannter Eintrag {id}")))?;
        self.entries[idx].label = label;
        Ok(())
    }

    /// Indizes der Einträge auf dem aktiven Pfad (root→leaf).
    fn path_indices(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut cur = self.leaf.clone();
        while let Some(id) = cur {
            match self.index.get(&id) {
                Some(&idx) => {
                    out.push(idx);
                    cur = self.entries[idx].parent_id.clone();
                }
                None => break,
            }
        }
        out.reverse();
        out
    }

    fn path_messages(&self) -> Vec<Message> {
        let path = self.path_indices();

        // Letzte Compaction auf dem Pfad bestimmt den Schnitt.
        let mut start = 0usize;
        let mut prefix: Option<Message> = None;
        for (pos, &idx) in path.iter().enumerate() {
            if let EntryPayload::Compaction {
                summary,
                replaced_until,
            } = &self.entries[idx].payload
            {
                let _ = pos;
                if let Some(rp) = path
                    .iter()
                    .position(|&i| &self.entries[i].id == replaced_until)
                {
                    start = rp + 1;
                    prefix = Some(summary_message(summary));
                }
            }
        }

        let mut out = Vec::new();
        if let Some(m) = prefix {
            out.push(m);
        }
        for &idx in &path[start..] {
            if let EntryPayload::Message { message } = &self.entries[idx].payload {
                out.push(message.clone());
            }
        }
        out
    }
}

/// In-Memory-Store (keine Persistenz).
#[derive(Debug)]
pub struct InMemorySessionStore {
    tree: Tree,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        InMemorySessionStore {
            tree: Tree::new(uuid::Uuid::new_v4().to_string()),
        }
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SessionStore for InMemorySessionStore {
    fn id(&self) -> &str {
        &self.tree.id
    }
    fn path_messages(&self) -> Vec<Message> {
        self.tree.path_messages()
    }
    fn append(&mut self, payload: EntryPayload) -> Result<EntryId> {
        Ok(self.tree.append_entry(payload).id)
    }
    fn branch(&mut self, to: &EntryId) -> Result<()> {
        self.tree.branch(to)
    }
    fn set_label(&mut self, id: &EntryId, label: Option<String>) -> Result<()> {
        self.tree.set_label(id, label)
    }
    fn entries(&self) -> &[Entry] {
        &self.tree.entries
    }
    fn leaf(&self) -> Option<&EntryId> {
        self.tree.leaf.as_ref()
    }
    async fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// JSONL-Store: append-only Datei, Durabilität an `flush()`.
#[derive(Debug)]
pub struct JsonlSessionStore {
    tree: Tree,
    path: PathBuf,
    writer: BufWriter<File>,
}

impl JsonlSessionStore {
    /// Legt eine neue Session in `project_dir` an (`<id>.jsonl`).
    pub fn create(project_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(project_dir)?;
        let id = uuid::Uuid::new_v4().to_string();
        let path = project_dir.join(format!("{id}.jsonl"));
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let header = Header {
            version: FORMAT_VERSION,
            session_id: id.clone(),
            cwd,
            created_at: now_millis(),
            name: None,
            parent_session: None,
        };
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)?;
        let mut writer = BufWriter::new(file);
        write_line(&mut writer, &Line::Header(header))?;
        Ok(JsonlSessionStore {
            tree: Tree::new(id),
            path,
            writer,
        })
    }

    /// Öffnet eine vorhandene Session-Datei.
    pub fn open(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut header: Option<Header> = None;
        let mut entries: Vec<Entry> = Vec::new();
        let mut labels: Vec<(EntryId, Option<String>)> = Vec::new();

        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            // Defekte (z. B. abgerissene letzte) Zeilen tolerieren.
            match serde_json::from_str::<Line>(line) {
                Ok(Line::Header(h)) => header = Some(h),
                Ok(Line::Entry(e)) => entries.push(e),
                Ok(Line::LabelChange { id, label }) => labels.push((id, label)),
                Err(_) => continue,
            }
        }

        let header =
            header.ok_or_else(|| SeppError::Session("Session-Datei ohne Header".into()))?;

        let mut index = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            index.insert(e.id.clone(), i);
        }
        for (id, label) in labels {
            if let Some(&idx) = index.get(&id) {
                entries[idx].label = label;
            }
        }
        // Aktiver Leaf = zuletzt angehängter Eintrag.
        let leaf = entries.last().map(|e| e.id.clone());

        let file = OpenOptions::new().append(true).open(path)?;
        Ok(JsonlSessionStore {
            tree: Tree {
                id: header.session_id.clone(),
                entries,
                index,
                leaf,
            },
            path: path.to_path_buf(),
            writer: BufWriter::new(file),
        })
    }

    /// Öffnet die zuletzt geänderte Session in `project_dir`.
    pub fn continue_recent(project_dir: &Path) -> Result<Self> {
        let newest = Self::list(project_dir)?
            .into_iter()
            .max_by_key(|s| s.modified)
            .ok_or_else(|| SeppError::Session("keine vorhandene Session gefunden".into()))?;
        Self::open(&newest.path)
    }

    /// Listet alle Sessions in `project_dir` (für Auswahl).
    pub fn list(project_dir: &Path) -> Result<Vec<SessionInfo>> {
        let mut out = Vec::new();
        let rd = match std::fs::read_dir(project_dir) {
            Ok(rd) => rd,
            Err(_) => return Ok(out), // Verzeichnis existiert noch nicht
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
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
}

fn read_info(path: &Path, modified: i64) -> Option<SessionInfo> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut header: Option<Header> = None;
    let mut entry_count = 0usize;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(line) {
            Ok(Line::Header(h)) => header = Some(h),
            Ok(Line::Entry(_)) => entry_count += 1,
            _ => {}
        }
    }
    let header = header?;
    Some(SessionInfo {
        id: header.session_id,
        path: path.to_path_buf(),
        created_at: header.created_at,
        modified,
        name: header.name,
        entry_count,
    })
}

fn write_line(writer: &mut BufWriter<File>, line: &Line) -> Result<()> {
    let s = serde_json::to_string(line)?;
    writer.write_all(s.as_bytes())?;
    writer.write_all(b"\n")?;
    Ok(())
}

#[async_trait::async_trait]
impl SessionStore for JsonlSessionStore {
    fn id(&self) -> &str {
        &self.tree.id
    }
    fn path_messages(&self) -> Vec<Message> {
        self.tree.path_messages()
    }
    fn append(&mut self, payload: EntryPayload) -> Result<EntryId> {
        let entry = self.tree.append_entry(payload);
        let id = entry.id.clone();
        write_line(&mut self.writer, &Line::Entry(entry))?;
        Ok(id)
    }
    fn branch(&mut self, to: &EntryId) -> Result<()> {
        self.tree.branch(to)
    }
    fn set_label(&mut self, id: &EntryId, label: Option<String>) -> Result<()> {
        self.tree.set_label(id, label.clone())?;
        write_line(
            &mut self.writer,
            &Line::LabelChange {
                id: id.clone(),
                label,
            },
        )
    }
    fn entries(&self) -> &[Entry] {
        &self.tree.entries
    }
    fn leaf(&self) -> Option<&EntryId> {
        self.tree.leaf.as_ref()
    }
    async fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> EntryPayload {
        EntryPayload::Message {
            message: Message::user_text(text),
        }
    }

    #[test]
    fn in_memory_linear_path() {
        let mut s = InMemorySessionStore::new();
        s.append(user("a")).unwrap();
        s.append(user("b")).unwrap();
        let msgs = s.path_messages();
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn branch_creates_sibling_keeps_history() {
        let mut s = InMemorySessionStore::new();
        let a = s.append(user("a")).unwrap();
        let _b = s.append(user("b")).unwrap();
        // Zurück auf a verzweigen, neuen Ast anhängen.
        s.branch(&a).unwrap();
        s.append(user("c")).unwrap();
        let msgs = s.path_messages();
        // Aktiver Pfad: a -> c (b ist anderer Ast, bleibt erhalten).
        assert_eq!(msgs.len(), 2);
        assert_eq!(s.entries().len(), 3); // nichts gelöscht
    }

    #[test]
    fn compaction_replaces_prefix() {
        let mut s = InMemorySessionStore::new();
        s.append(user("alt 1")).unwrap();
        let cut = s.append(user("alt 2")).unwrap();
        s.append(EntryPayload::Compaction {
            summary: "Z".into(),
            replaced_until: cut,
        })
        .unwrap();
        s.append(user("neu")).unwrap();
        let msgs = s.path_messages();
        // [summary, "neu"]
        assert_eq!(msgs.len(), 2);
        assert!(matches!(&msgs[0].content[0],
            ContentBlock::Text { text } if text.contains("Z")));
    }

    #[tokio::test]
    async fn jsonl_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let id;
        {
            let mut s = JsonlSessionStore::create(dir.path()).unwrap();
            s.append(user("hallo")).unwrap();
            s.append(user("welt")).unwrap();
            s.flush().await.unwrap();
            id = s.id().to_string();
        }
        let recent = JsonlSessionStore::continue_recent(dir.path()).unwrap();
        assert_eq!(recent.id(), id);
        assert_eq!(recent.path_messages().len(), 2);
        assert_eq!(JsonlSessionStore::list(dir.path()).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn jsonl_tolerates_torn_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let path;
        {
            let mut s = JsonlSessionStore::create(dir.path()).unwrap();
            s.append(user("ok")).unwrap();
            s.flush().await.unwrap();
            path = s.path().to_path_buf();
        }
        // Eine kaputte halbe Zeile anhängen.
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"{\"type\":\"entry\",\"id\":\"x\",\"par")
                .unwrap();
        }
        let s = JsonlSessionStore::open(&path).unwrap();
        assert_eq!(s.path_messages().len(), 1); // kaputte Zeile verworfen
    }

    #[tokio::test]
    async fn labels_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path;
        let eid;
        {
            let mut s = JsonlSessionStore::create(dir.path()).unwrap();
            eid = s.append(user("x")).unwrap();
            s.set_label(&eid, Some("checkpoint".into())).unwrap();
            s.flush().await.unwrap();
            path = s.path().to_path_buf();
        }
        let s = JsonlSessionStore::open(&path).unwrap();
        let labeled = s.entries().iter().find(|e| e.id == eid).unwrap();
        assert_eq!(labeled.label.as_deref(), Some("checkpoint"));
    }
}
