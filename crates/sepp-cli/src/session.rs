//! Session-Ablage und Resource-/Trust-Wurzeln unter `~/.sepp` bzw. `<repo>/.sepp`.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use anyhow::{anyhow, Result};

use sepp_session::{JsonlSessionStore, SessionInfo, SessionStore};

/// Welche Session beim Start verwendet wird.
#[derive(Debug, Clone)]
pub enum SessionSelect {
    New,
    Continue,
    Resume(Option<String>),
}

fn home() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .ok_or_else(|| anyhow!("Home-Verzeichnis nicht ermittelbar"))
}

/// `~/.sepp` — globale Wurzel für Sessions, Resources, Hooks, Trust.
pub fn sepp_root() -> Result<PathBuf> {
    Ok(home()?.join(".sepp"))
}

fn cwd_canon() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    Ok(cwd.canonicalize().unwrap_or(cwd))
}

/// `~/.sepp/sessions/<hash(cwd)>` — stabil über Prozessläufe (fester DefaultHasher).
pub fn project_session_dir() -> Result<PathBuf> {
    let mut h = DefaultHasher::new();
    cwd_canon()?.hash(&mut h);
    Ok(sepp_root()?
        .join("sessions")
        .join(format!("{:016x}", h.finish())))
}

/// Resource-Wurzeln (jede enthält optional `skills/`, `prompts/`, `themes/`): global immer,
/// projektlokal nur, wenn das Projekt vertraut ist.
pub fn resource_roots(project_trusted: bool) -> Result<Vec<PathBuf>> {
    let mut roots = vec![sepp_root()?];
    if project_trusted {
        roots.push(std::env::current_dir()?.join(".sepp"));
    }
    Ok(roots)
}

/// Hook-Verzeichnisse (`<root>/hooks`): global immer, projektlokal nur nach Trust.
pub fn hook_dirs(project_trusted: bool) -> Result<Vec<PathBuf>> {
    let mut dirs = vec![sepp_root()?.join("hooks")];
    if project_trusted {
        dirs.push(std::env::current_dir()?.join(".sepp").join("hooks"));
    }
    Ok(dirs)
}

/// `settings.toml`-Pfade (`[[mcp.servers]]`): global immer, projektlokal nur nach Trust.
pub fn settings_paths(project_trusted: bool) -> Result<Vec<PathBuf>> {
    let mut paths = vec![sepp_root()?.join("settings.toml")];
    if project_trusted {
        paths.push(std::env::current_dir()?.join(".sepp").join("settings.toml"));
    }
    Ok(paths)
}

/// WASM-Plugin-Verzeichnisse (`<root>/plugins`): global immer, projektlokal nur nach Trust.
pub fn plugin_dirs(project_trusted: bool) -> Result<Vec<PathBuf>> {
    let mut dirs = vec![sepp_root()?.join("plugins")];
    if project_trusted {
        dirs.push(std::env::current_dir()?.join(".sepp").join("plugins"));
    }
    Ok(dirs)
}

// ---- Trust (Vorstufe zu sepp-policy, Phase 4) ---------------------------

fn trust_file() -> Result<PathBuf> {
    Ok(sepp_root()?.join("trust.json"))
}

fn read_trust() -> Result<HashMap<String, bool>> {
    match std::fs::read_to_string(trust_file()?) {
        Ok(s) => Ok(serde_json::from_str(&s).unwrap_or_default()),
        Err(_) => Ok(HashMap::new()),
    }
}

/// Ist das aktuelle Projekt (cwd) vertraut?
pub fn is_project_trusted() -> Result<bool> {
    let key = cwd_canon()?.display().to_string();
    Ok(read_trust()?.get(&key).copied().unwrap_or(false))
}

/// Markiert das aktuelle Projekt als vertraut (persistiert in `~/.sepp/trust.json`).
pub fn trust_current_project() -> Result<()> {
    let key = cwd_canon()?.display().to_string();
    let mut map = read_trust()?;
    map.insert(key, true);
    let path = trust_file()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&map)?)?;
    Ok(())
}

// ---- Session-Stores -----------------------------------------------------

pub fn list_sessions() -> Result<Vec<SessionInfo>> {
    Ok(JsonlSessionStore::list(&project_session_dir()?)?)
}

pub fn new_store() -> Result<Box<dyn SessionStore>> {
    Ok(Box::new(
        JsonlSessionStore::create(&project_session_dir()?)?,
    ))
}

pub fn open_store(select: &SessionSelect) -> Result<Box<dyn SessionStore>> {
    let dir = project_session_dir()?;
    let store: Box<dyn SessionStore> = match select {
        SessionSelect::New => Box::new(JsonlSessionStore::create(&dir)?),
        SessionSelect::Continue => Box::new(JsonlSessionStore::continue_recent(&dir)?),
        SessionSelect::Resume(Some(id)) => {
            let info = JsonlSessionStore::list(&dir)?
                .into_iter()
                .find(|s| s.id.starts_with(id.as_str()))
                .ok_or_else(|| anyhow!("keine Session mit Präfix '{id}' gefunden"))?;
            Box::new(JsonlSessionStore::open(&info.path)?)
        }
        SessionSelect::Resume(None) => Box::new(JsonlSessionStore::continue_recent(&dir)?),
    };
    Ok(store)
}

/// Wie [`open_store`], aber mit dem SQLite-Backend (`.sqlite`-Dateien).
#[cfg(feature = "sqlite")]
pub fn sqlite_store(select: &SessionSelect) -> Result<Box<dyn SessionStore>> {
    use sepp_session::SqliteSessionStore;
    let dir = project_session_dir()?;
    let store: Box<dyn SessionStore> = match select {
        SessionSelect::New => Box::new(SqliteSessionStore::create(&dir)?),
        SessionSelect::Continue => Box::new(SqliteSessionStore::continue_recent(&dir)?),
        SessionSelect::Resume(Some(id)) => {
            let info = SqliteSessionStore::list(&dir)?
                .into_iter()
                .find(|s| s.id.starts_with(id.as_str()))
                .ok_or_else(|| anyhow!("keine SQLite-Session mit Präfix '{id}' gefunden"))?;
            Box::new(SqliteSessionStore::open(&info.path)?)
        }
        SessionSelect::Resume(None) => Box::new(SqliteSessionStore::continue_recent(&dir)?),
    };
    Ok(store)
}
