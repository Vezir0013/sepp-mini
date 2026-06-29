//! Pfad-Auflösung für sepp: getrennte **config**- und **state**-Wurzeln (FHS-fähig) plus die
//! projektlokale Config-Wurzel `<repo>/.sepp`.
//!
//! - **config_root** (settings.toml, skills/, prompts/, hooks/, plugins/; künftig auth.json, 0600):
//!   `$SEPP_CONFIG_DIR` → `$SEPP_HOME` → vorhandenes `~/.sepp` → vorhandenes `/etc/sepp` → `~/.sepp`.
//! - **state_root** (sessions/, trust.json): analog mit `$SEPP_STATE_DIR` und `/var/lib/sepp`.
//!
//! Für normale Nutzer bleibt es damit die **eine** Wurzel `~/.sepp`; der FHS-Split greift nur, wenn
//! die Env-Variablen gesetzt sind oder ein System-Setup unter `/etc/sepp` + `/var/lib/sepp` existiert
//! (`sepp init --system`). `SEPP_HOME` setzt weiterhin beide Wurzeln zugleich (Rückwärtskompatibel).

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use sepp_session::{JsonlSessionStore, SessionInfo, SessionStore};

/// Welche Session beim Start verwendet wird.
#[derive(Debug, Clone)]
pub enum SessionSelect {
    New,
    Continue,
    Resume(Option<String>),
}

/// Ziel von `sepp init`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitScope {
    /// Projektlokal `<cwd>/.sepp` (nur Config), wird auto-vertraut.
    Project,
    /// Globale Nutzer-Wurzel (Default `~/.sepp`, via Env verlegbar).
    Global,
    /// System-Installation (Default `/etc/sepp` + `/var/lib/sepp`, via Env verlegbar).
    System,
}

fn home() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .ok_or_else(|| anyhow!("Home-Verzeichnis nicht ermittelbar"))
}

/// Reine Wurzel-Auflösung: explizite Variable > `SEPP_HOME`-Alias > Default. Leere Werte zählen als
/// nicht gesetzt (Konvention wie bei `SEPP_HOME`).
fn resolve_root(specific: Option<&OsStr>, home_alias: Option<&OsStr>, default: PathBuf) -> PathBuf {
    if let Some(v) = specific.filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    if let Some(v) = home_alias.filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    default
}

/// Reiner Laufzeit-Default: bevorzugt die vorhandene User-Wurzel, fällt sonst auf eine vorhandene
/// System-Wurzel zurück (so findet ein `sepp init --system`-Setup sich auch ohne gesetzte Env),
/// sonst die User-Wurzel.
fn runtime_default(user: PathBuf, system: &Path, exists: impl Fn(&Path) -> bool) -> PathBuf {
    if exists(&user) {
        user
    } else if exists(system) {
        system.to_path_buf()
    } else {
        user
    }
}

/// Liest `$<specific_var>` und `$SEPP_HOME` und wendet [`resolve_root`] mit dem gegebenen Default an.
fn resolve_named(specific_var: &str, default: PathBuf) -> PathBuf {
    let specific = std::env::var_os(specific_var);
    let alias = std::env::var_os("SEPP_HOME");
    resolve_root(specific.as_deref(), alias.as_deref(), default)
}

/// Config-Wurzel: `settings.toml`, `skills/`, `prompts/`, `hooks/`, `plugins/` (künftig `auth.json`).
pub fn config_root() -> Result<PathBuf> {
    let default = runtime_default(home()?.join(".sepp"), Path::new("/etc/sepp"), |p| {
        p.exists()
    });
    Ok(resolve_named("SEPP_CONFIG_DIR", default))
}

/// State-Wurzel: `sessions/`, `trust.json`.
pub fn state_root() -> Result<PathBuf> {
    let default = runtime_default(home()?.join(".sepp"), Path::new("/var/lib/sepp"), |p| {
        p.exists()
    });
    Ok(resolve_named("SEPP_STATE_DIR", default))
}

/// `(config, state)`-Wurzeln, die `sepp init <scope>` anlegt. `Project` liefert nur die config-Wurzel
/// (`<cwd>/.sepp`); Sessions/Trust liegen zentral im state_root, daher dort kein eigener State.
pub fn init_roots(scope: InitScope) -> Result<(PathBuf, Option<PathBuf>)> {
    match scope {
        InitScope::Project => Ok((project_root()?, None)),
        InitScope::Global => Ok((
            resolve_named("SEPP_CONFIG_DIR", home()?.join(".sepp")),
            Some(resolve_named("SEPP_STATE_DIR", home()?.join(".sepp"))),
        )),
        InitScope::System => Ok((
            resolve_named("SEPP_CONFIG_DIR", PathBuf::from("/etc/sepp")),
            Some(resolve_named(
                "SEPP_STATE_DIR",
                PathBuf::from("/var/lib/sepp"),
            )),
        )),
    }
}

/// Projektlokale **Config**-Wurzel `<cwd>/.sepp` (nur skills/prompts/hooks/plugins/settings.toml;
/// lädt erst nach `/trust`). Sessions und Trust liegen NICHT hier, sondern zentral im state_root.
/// Nicht kanonisiert — spiegelt die Loader, damit `sepp init` und das Laden denselben Pfad treffen.
pub fn project_root() -> Result<PathBuf> {
    Ok(std::env::current_dir()?.join(".sepp"))
}

fn cwd_canon() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    Ok(cwd.canonicalize().unwrap_or(cwd))
}

/// `state_root()/sessions/<hash(cwd)>` — zentral, pro Arbeitsverzeichnis getrennt (stabiler
/// `DefaultHasher`). Sessions sind State und liegen damit unter der state-Wurzel, nicht projektlokal.
pub fn project_session_dir() -> Result<PathBuf> {
    let mut h = DefaultHasher::new();
    cwd_canon()?.hash(&mut h);
    Ok(state_root()?
        .join("sessions")
        .join(format!("{:016x}", h.finish())))
}

/// Resource-Wurzeln (jede enthält optional `skills/`, `prompts/`, `themes/`): global immer,
/// projektlokal nur, wenn das Projekt vertraut ist.
pub fn resource_roots(project_trusted: bool) -> Result<Vec<PathBuf>> {
    let mut roots = vec![config_root()?];
    if project_trusted {
        roots.push(project_root()?);
    }
    Ok(roots)
}

/// Hook-Verzeichnisse (`<config_root>/hooks`): global immer, projektlokal nur nach Trust.
pub fn hook_dirs(project_trusted: bool) -> Result<Vec<PathBuf>> {
    let mut dirs = vec![config_root()?.join("hooks")];
    if project_trusted {
        dirs.push(project_root()?.join("hooks"));
    }
    Ok(dirs)
}

/// `settings.toml`-Pfade (`[[mcp.servers]]`): global immer, projektlokal nur nach Trust.
pub fn settings_paths(project_trusted: bool) -> Result<Vec<PathBuf>> {
    let mut paths = vec![config_root()?.join("settings.toml")];
    if project_trusted {
        paths.push(project_root()?.join("settings.toml"));
    }
    Ok(paths)
}

/// WASM-Plugin-Verzeichnisse (`<config_root>/plugins`): global immer, projektlokal nur nach Trust.
pub fn plugin_dirs(project_trusted: bool) -> Result<Vec<PathBuf>> {
    let mut dirs = vec![config_root()?.join("plugins")];
    if project_trusted {
        dirs.push(project_root()?.join("plugins"));
    }
    Ok(dirs)
}

// ---- Trust (Vorstufe zu sepp-policy, Phase 4) ---------------------------

fn trust_file() -> Result<PathBuf> {
    Ok(state_root()?.join("trust.json"))
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

/// Alle als vertraut markierten Projektpfade (kanonische cwd, in denen `sepp init` projektlokal
/// lief). Dient `uninstall --purge` als Anker, um projektlokale `.sepp`-Verzeichnisse
/// standortunabhängig zu finden. Leere/fehlende `trust.json` ⇒ leere Liste.
pub fn trusted_projects() -> Result<Vec<PathBuf>> {
    Ok(read_trust()?
        .into_iter()
        .filter(|(_, trusted)| *trusted)
        .map(|(path, _)| PathBuf::from(path))
        .collect())
}

/// Markiert das aktuelle Projekt als vertraut (persistiert in `state_root()/trust.json`).
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
    fn resolve_root_precedence() {
        let d = PathBuf::from("/default");
        // explizit schlägt alles
        assert_eq!(
            resolve_root(
                Some(OsStr::new("/spec")),
                Some(OsStr::new("/home")),
                d.clone()
            ),
            PathBuf::from("/spec")
        );
        // ohne explizit greift der SEPP_HOME-Alias
        assert_eq!(
            resolve_root(None, Some(OsStr::new("/home")), d.clone()),
            PathBuf::from("/home")
        );
        // leere Werte zählen als nicht gesetzt
        assert_eq!(
            resolve_root(Some(OsStr::new("")), Some(OsStr::new("/home")), d.clone()),
            PathBuf::from("/home")
        );
        assert_eq!(resolve_root(None, None, d.clone()), d);
    }

    #[test]
    fn runtime_default_prefers_user_then_system() {
        let user = PathBuf::from("/home/u/.sepp");
        let sys = Path::new("/etc/sepp");
        // User-Wurzel existiert → User
        assert_eq!(runtime_default(user.clone(), sys, |_| true), user);
        // nur System existiert → System
        assert_eq!(
            runtime_default(user.clone(), sys, |p| p == sys),
            sys.to_path_buf()
        );
        // nichts existiert → User-Default
        assert_eq!(runtime_default(user.clone(), sys, |_| false), user);
    }

    #[test]
    fn roots_honor_env_overrides() {
        // Einziger Test im sepp-cli-Binary, der Prozess-Env mutiert → bewusst seriell gehalten.
        // Erst die System-Defaults bei sauberer Env (keine SEPP_*-Variablen gesetzt), dann Overrides.
        let (c0, s0) = init_roots(InitScope::System).unwrap();
        assert_eq!(c0, PathBuf::from("/etc/sepp"));
        assert_eq!(s0, Some(PathBuf::from("/var/lib/sepp")));

        std::env::set_var("SEPP_HOME", "/tmp/sepp-home-x");
        assert_eq!(config_root().unwrap(), PathBuf::from("/tmp/sepp-home-x"));
        assert_eq!(state_root().unwrap(), PathBuf::from("/tmp/sepp-home-x"));
        // spezifische Variablen schlagen den SEPP_HOME-Alias
        std::env::set_var("SEPP_CONFIG_DIR", "/tmp/cfg-x");
        std::env::set_var("SEPP_STATE_DIR", "/tmp/state-x");
        assert_eq!(config_root().unwrap(), PathBuf::from("/tmp/cfg-x"));
        assert_eq!(state_root().unwrap(), PathBuf::from("/tmp/state-x"));
        // System-Init-Ziele folgen denselben Overrides
        let (c, s) = init_roots(InitScope::System).unwrap();
        assert_eq!(c, PathBuf::from("/tmp/cfg-x"));
        assert_eq!(s, Some(PathBuf::from("/tmp/state-x")));
        for k in ["SEPP_HOME", "SEPP_CONFIG_DIR", "SEPP_STATE_DIR"] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn project_init_is_config_only() {
        let (c, s) = init_roots(InitScope::Project).unwrap();
        assert_eq!(c, project_root().unwrap());
        assert!(s.is_none(), "Projekt-Init hat keinen eigenen State-Root");
    }

    #[test]
    fn project_root_is_cwd_dot_sepp() {
        let root = project_root().unwrap();
        assert!(root.ends_with(".sepp"));
        assert_eq!(root, std::env::current_dir().unwrap().join(".sepp"));
    }

    #[test]
    fn project_session_dir_is_state_root_sessions_hash() {
        // Zentral unter state_root/sessions/<16-hex>. Struktur prüfen (nicht den absoluten state_root,
        // der env-/fs-abhängig ist).
        let dir = project_session_dir().unwrap();
        let hash = dir.file_name().unwrap().to_str().unwrap();
        assert_eq!(hash.len(), 16, "16-stelliger Hex-Hash");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(dir.parent().unwrap().ends_with("sessions"));
    }
}
