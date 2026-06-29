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

/// Globale Wurzel für Sessions, Resources, Hooks, Trust. Default `~/.sepp`; über die
/// Umgebungsvariable `SEPP_HOME` direkt verlegbar (Konvention wie `CARGO_HOME` — der Wert IST die
/// Wurzel, kein `.sepp` wird angehängt). Einzige Quelle für init/uninstall **und** alle Loader,
/// daher zieht der Override überall konsistent mit (auch `trust.json`).
pub fn sepp_root() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("SEPP_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(home()?.join(".sepp"))
}

/// Projektlokale Wurzel `<cwd>/.sepp` (Erweiterungen hier laden nur nach `/trust`). Nicht
/// kanonisiert — spiegelt die Loader, damit `sepp init` und das Laden denselben Pfad treffen.
pub fn project_root() -> Result<PathBuf> {
    Ok(std::env::current_dir()?.join(".sepp"))
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
        roots.push(project_root()?);
    }
    Ok(roots)
}

/// Hook-Verzeichnisse (`<root>/hooks`): global immer, projektlokal nur nach Trust.
pub fn hook_dirs(project_trusted: bool) -> Result<Vec<PathBuf>> {
    let mut dirs = vec![sepp_root()?.join("hooks")];
    if project_trusted {
        dirs.push(project_root()?.join("hooks"));
    }
    Ok(dirs)
}

/// `settings.toml`-Pfade (`[[mcp.servers]]`): global immer, projektlokal nur nach Trust.
pub fn settings_paths(project_trusted: bool) -> Result<Vec<PathBuf>> {
    let mut paths = vec![sepp_root()?.join("settings.toml")];
    if project_trusted {
        paths.push(project_root()?.join("settings.toml"));
    }
    Ok(paths)
}

/// WASM-Plugin-Verzeichnisse (`<root>/plugins`): global immer, projektlokal nur nach Trust.
pub fn plugin_dirs(project_trusted: bool) -> Result<Vec<PathBuf>> {
    let mut dirs = vec![sepp_root()?.join("plugins")];
    if project_trusted {
        dirs.push(project_root()?.join("plugins"));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sepp_root_honors_sepp_home_override() {
        // Einziger Test im sepp-cli-Binary, der SEPP_HOME berührt → keine Race mit anderen.
        std::env::set_var("SEPP_HOME", "/tmp/sepp-home-test");
        assert_eq!(sepp_root().unwrap(), PathBuf::from("/tmp/sepp-home-test"));
        // Leerer Wert zählt als nicht gesetzt → Default-Pfad endet auf ".sepp".
        std::env::set_var("SEPP_HOME", "");
        assert!(sepp_root().unwrap().ends_with(".sepp"));
        std::env::remove_var("SEPP_HOME");
        assert!(sepp_root().unwrap().ends_with(".sepp"));
    }

    #[test]
    fn project_root_is_cwd_dot_sepp() {
        let root = project_root().unwrap();
        assert!(root.ends_with(".sepp"));
        assert_eq!(root, std::env::current_dir().unwrap().join(".sepp"));
    }
}
