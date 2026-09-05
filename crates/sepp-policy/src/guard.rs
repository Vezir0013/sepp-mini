//! Sepp Guard — **ein Regelwerk, ein Entscheider, ein Audit, mehrere Vollstrecker.**
//!
//! - **Regelwerk**: die Policy-Datei ([`PolicyFile`]) aus `~/.sepp/settings.toml` (`[policy]`),
//!   `~/.sepp/policy.toml` und `<repo>/.sepp/policy.toml` (nur nach Trust). Die Quellen werden zu
//!   einem [`PolicySet`] vereinigt; `[deny]` gewinnt immer. Die eingebauten Agent-Defaults
//!   ([`builtin_agent_grants`]) gelten immer — Dateien **erweitern** sie, einschränken geht über
//!   `[deny]`.
//! - **Entscheider**: [`Guard::decide`] (pur) und [`Guard::authorize`] (mit Rückfrage über einen
//!   [`PermissionPrompter`], sofern vorhanden) — eine Stelle, die für jeden Akteur ([`Actor`]) und
//!   jede Aktion ([`Action`]) sagt: erlaubt, fragen, verweigern.
//! - **Audit**: jede Entscheidung landet als [`AuditEvent`] im Guard; Tools legen sie zusätzlich
//!   in `ToolResult.details["guard"]`, damit Frontends und der Session-Store sie sehen.
//! - **Vollstrecker**: der [`Sandbox`]-Adapter (Landlock/Seatbelt) für Kindprozesse, die
//!   Pfadprüfung ([`Policy::allows_path`]) für die eingebauten Tools, das wasmi-Linker-Gate für
//!   WASM-Plugins. Landlock kann Verbote unterhalb einer Gewährung nicht ausdrücken; solche
//!   Überlappungen meldet [`PolicySet::deny_overlaps`].
//!
//! Modus-Tabelle (Vertrag, siehe Tests):
//!
//! | Modus  | innerhalb der Policy | außerhalb der Policy                | Rückfrage-Muster |
//! |--------|----------------------|-------------------------------------|------------------|
//! | `ask`  | erlaubt              | fragt (ohne Prompter: verweigert)   | fragt            |
//! | `auto` | erlaubt              | verweigert                          | ignoriert        |
//! | `yolo` | Guard nicht aktiv (alles erlaubt, keine Sandbox)                                  |

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use sepp_core::{Result, SeppError};

use crate::{
    canonicalize_lenient, resolve_path_with, system_read_paths, Capability, DenyOverlap, DenyRule,
    Policy, ResolveCtx, Sandbox,
};

// ---------------------------------------------------------------------------------------------
// Regelwerk: Wire-Format der Policy-Datei
// ---------------------------------------------------------------------------------------------

/// Betriebsmodus des Guards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Innerhalb der Policy erlaubt, außerhalb Rückfrage (ohne Dialog: verweigert).
    #[default]
    Ask,
    /// Innerhalb der Policy erlaubt, außerhalb verweigert — ohne Rückfrage.
    Auto,
    /// Guard aus: heutiges Verhalten ohne Sandbox für die eingebauten Tools.
    Yolo,
}

impl FromStr for Mode {
    type Err = SeppError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" => Ok(Mode::Ask),
            "auto" => Ok(Mode::Auto),
            "yolo" => Ok(Mode::Yolo),
            other => Err(SeppError::Config(format!(
                "unbekannter Modus '{other}' (erlaubt: ask | auto | yolo)"
            ))),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Ask => "ask",
            Mode::Auto => "auto",
            Mode::Yolo => "yolo",
        })
    }
}

/// `net = false | true | ["host", …]`. Für Kindprozesse gilt jede Gewährung als „TCP gesamt"
/// (Landlock/Seatbelt filtern nicht nach Host); die Host-Liste dient dem künftigen Egress-Proxy.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(from = "NetGrantRaw")]
pub enum NetGrant {
    #[default]
    Off,
    All,
    Hosts(Vec<String>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NetGrantRaw {
    Flag(bool),
    Hosts(Vec<String>),
}

impl From<NetGrantRaw> for NetGrant {
    fn from(raw: NetGrantRaw) -> Self {
        match raw {
            NetGrantRaw::Flag(true) => NetGrant::All,
            NetGrantRaw::Flag(false) => NetGrant::Off,
            NetGrantRaw::Hosts(v) if v.is_empty() => NetGrant::Off,
            NetGrantRaw::Hosts(v) => NetGrant::Hosts(v),
        }
    }
}

/// `exec = "system" | ["git", …]`. `Unset` heißt „keine Meinung" (fehlendes Feld), damit eine
/// Quelle ohne `exec`-Zeile eine Liste aus einer anderen Quelle nicht still aufhebt.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(try_from = "ExecGrantRaw")]
pub enum ExecGrant {
    #[default]
    Unset,
    /// Alle Systemprogramme (keine Exec-Beschränkung).
    System,
    /// Nur diese Programme (plus das gestartete Programm selbst).
    Programs(Vec<String>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ExecGrantRaw {
    Word(String),
    Programs(Vec<String>),
}

impl TryFrom<ExecGrantRaw> for ExecGrant {
    type Error = String;
    fn try_from(raw: ExecGrantRaw) -> std::result::Result<Self, Self::Error> {
        match raw {
            ExecGrantRaw::Word(w) if w == "system" => Ok(ExecGrant::System),
            ExecGrantRaw::Word(other) => Err(format!(
                "exec: unbekanntes Schlüsselwort '{other}' (erlaubt: \"system\" oder eine Liste)"
            )),
            ExecGrantRaw::Programs(v) => Ok(ExecGrant::Programs(v)),
        }
    }
}

/// Gewährungen eines Abschnitts (`[agent]`, `[mcp.<name>]`, `[plugin.<name>]`, `[deny]`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct Grants {
    /// Lesepräfixe; `"system"` expandiert zu den Systempfaden ([`system_read_paths`]).
    pub fs_read: Vec<String>,
    pub fs_write: Vec<String>,
    pub exec: ExecGrant,
    pub net: NetGrant,
    pub env: Vec<String>,
}

impl Grants {
    /// Enthält der Abschnitt überhaupt eine Gewährung?
    pub fn is_empty(&self) -> bool {
        self.fs_read.is_empty()
            && self.fs_write.is_empty()
            && self.exec == ExecGrant::Unset
            && self.net == NetGrant::Off
            && self.env.is_empty()
    }

    /// Baut eine [`Policy`] (Pfade aufgelöst, `"system"` expandiert). `Unset`/`System` bei
    /// `exec` ergeben keine `Exec`-Einträge (= unbeschränkt).
    pub fn to_policy_with(&self, ctx: &ResolveCtx) -> Policy {
        let mut granted = Vec::new();
        for raw in &self.fs_read {
            if raw == "system" {
                for s in system_read_paths() {
                    granted.push(Capability::FsRead {
                        prefix: PathBuf::from(s),
                    });
                }
            } else {
                granted.push(Capability::FsRead {
                    prefix: resolve_path_with(raw, ctx),
                });
            }
        }
        for raw in &self.fs_write {
            granted.push(Capability::FsWrite {
                prefix: resolve_path_with(raw, ctx),
            });
        }
        if let ExecGrant::Programs(progs) = &self.exec {
            for p in progs {
                granted.push(Capability::Exec { program: p.clone() });
            }
        }
        match &self.net {
            NetGrant::Off => {}
            NetGrant::All => granted.push(Capability::Net { host: "*".into() }),
            NetGrant::Hosts(hosts) => {
                for h in hosts {
                    granted.push(Capability::Net { host: h.clone() });
                }
            }
        }
        for e in &self.env {
            granted.push(Capability::Env { name: e.clone() });
        }
        Policy::new(granted)
    }

    /// Wie [`Grants::to_policy_with`] mit dem Prozess-Kontext.
    pub fn to_policy(&self) -> Policy {
        self.to_policy_with(&ResolveCtx::from_env())
    }
}

/// `[agent.ask]`: Rückfrage-Muster (Substring auf dem Shell-Kommando). Komfort, keine
/// Sicherheitsgrenze — die Grenze ist die Policy, vom Kernel durchgesetzt.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct AskRules {
    pub patterns: Vec<String>,
}

/// `[agent]`: Gewährungen des Agenten plus `[agent.ask]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct AgentSection {
    #[serde(flatten)]
    pub grants: Grants,
    #[serde(default)]
    pub ask: AskRules,
}

/// Eine Policy-Datei (Wire-Format, nur additiv erweitern).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct PolicyFile {
    pub mode: Option<Mode>,
    pub agent: Option<AgentSection>,
    pub mcp: BTreeMap<String, Grants>,
    pub plugin: BTreeMap<String, Grants>,
    /// Nur `fs_read` (sperrt Lesen und Schreiben) und `fs_write` (sperrt Schreiben) werden
    /// ausgewertet; andere Felder erzeugen eine Warnung.
    pub deny: Grants,
}

impl PolicyFile {
    /// Parst eine `policy.toml`.
    pub fn parse(text: &str) -> Result<PolicyFile> {
        toml::from_str(text).map_err(|e| SeppError::Config(format!("policy: {e}")))
    }

    /// Liest den Abschnitt `[policy]` aus einer `settings.toml`; andere Schlüssel (z. B.
    /// `[[mcp.servers]]`) werden ignoriert. `Ok(None)`, wenn kein `[policy]` vorhanden ist.
    pub fn from_settings_toml(text: &str) -> Result<Option<PolicyFile>> {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            policy: Option<PolicyFile>,
        }
        toml::from_str::<Wrapper>(text)
            .map(|w| w.policy)
            .map_err(|e| SeppError::Config(format!("settings [policy]: {e}")))
    }
}

/// Eingebaute Verbote (relativ zu `~`), immer aktiv — der Agent darf seine Schlüssel und seine
/// eigene Konfiguration/Sessions nicht lesen.
pub const BUILTIN_DENY: &[&str] = &["~/.ssh", "~/.aws", "~/.gnupg", "~/.sepp"];

/// Eingebaute `[agent]`-Gewährungen (gelten immer, Dateien erweitern sie): Projekt und
/// Systempfade lesbar, Projekt und `$TMPDIR` schreibbar, Ausführen unbeschränkt (solange keine
/// Quelle eine `exec`-Liste setzt), kein Netz, minimale Umgebung.
pub fn builtin_agent_grants() -> Grants {
    Grants {
        fs_read: vec!["./".into(), "system".into()],
        fs_write: vec!["./".into(), "$TMPDIR".into()],
        exec: ExecGrant::Unset,
        net: NetGrant::Off,
        env: [
            "PATH", "HOME", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TMPDIR",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
    }
}

// ---------------------------------------------------------------------------------------------
// Zusammenführung: PolicySet
// ---------------------------------------------------------------------------------------------

/// Woher eine Regel stammt (für `sepp policy` und Fehlermeldungen).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Builtin,
    File(PathBuf),
    Cli,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Builtin => f.write_str("eingebaut"),
            Source::File(p) => write!(f, "{}", p.display()),
            Source::Cli => f.write_str("--mode/SEPP_MODE"),
        }
    }
}

/// Wer eine Aktion ausführen will.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Actor {
    /// Die eingebauten Tools des Modells (bash, read, write, edit) und der Sub-Agent `task`.
    Agent,
    Mcp(String),
    Plugin(String),
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Actor::Agent => f.write_str("agent"),
            Actor::Mcp(n) => write!(f, "mcp {n}"),
            Actor::Plugin(n) => write!(f, "plugin {n}"),
        }
    }
}

/// Eine gewährte Capability samt Herkunft und Rohtext (für die Anzeige).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantEntry {
    pub actor: Actor,
    pub cap: Capability,
    /// Der Wert, wie er in der Quelle stand (`"./"`, `"system"`, `"true"`, …).
    pub raw: String,
    pub source: Source,
}

/// Art einer Policy-Quelle: `settings.toml` (Abschnitt `[policy]`) oder eine reine `policy.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    SettingsToml,
    PolicyToml,
}

/// Eine Datei, aus der Policy geladen wird (fehlende Dateien werden übersprungen).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySource {
    pub path: PathBuf,
    pub kind: SourceKind,
}

/// Vom Frontend beigesteuerte Defaults: zusätzliche Verbote (config_root, state_root) und der
/// Modus, wenn keine Quelle einen setzt (TUI: `ask`, `-p`/`--rpc`: `auto`).
#[derive(Debug, Clone, Default)]
pub struct BuiltinDefaults {
    pub extra_deny: Vec<PathBuf>,
    pub default_mode: Mode,
}

/// Das zusammengeführte Regelwerk: Vereinigung aller Gewährungen mit Herkunft, alle Verbote,
/// Rückfrage-Muster, Warnungen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySet {
    pub mode: Mode,
    pub mode_source: Source,
    pub entries: Vec<GrantEntry>,
    /// Akteure, für die eine Quelle `exec = "system"` gesetzt hat (Exec unbeschränkt).
    pub exec_open: Vec<Actor>,
    pub deny: Vec<(DenyRule, Source)>,
    pub ask_patterns: Vec<(String, Source)>,
    pub warnings: Vec<String>,
    /// Alle betrachteten Quellen und ob sie existierten (für `sepp policy`).
    pub sources: Vec<(PolicySource, bool)>,
}

impl PolicySet {
    /// Führt geladene Dateien (in Ladereihenfolge) mit den Defaults zusammen. Pur — die
    /// Dateien liest [`load_policy_set`].
    pub fn merge(
        files: Vec<(Source, PolicyFile)>,
        defaults: &BuiltinDefaults,
        mode_override: Option<Mode>,
        ctx: &ResolveCtx,
    ) -> PolicySet {
        let mut set = PolicySet {
            mode: defaults.default_mode,
            mode_source: Source::Builtin,
            entries: Vec::new(),
            exec_open: Vec::new(),
            deny: Vec::new(),
            ask_patterns: Vec::new(),
            warnings: Vec::new(),
            sources: Vec::new(),
        };

        // Eingebaute Agent-Gewährungen gelten immer; Dateien erweitern sie (einschränken: [deny]).
        set.push_grants(
            &Actor::Agent,
            &builtin_agent_grants(),
            &Source::Builtin,
            ctx,
        );
        // Eingebaute Verbote immer.
        for d in BUILTIN_DENY {
            set.deny
                .push((DenyRule::read(resolve_path_with(d, ctx)), Source::Builtin));
        }
        for d in &defaults.extra_deny {
            let rule = DenyRule::read(d.clone());
            if !set.deny.iter().any(|(r, _)| *r == rule) {
                set.deny.push((rule, Source::Builtin));
            }
        }

        for (source, file) in &files {
            if let Some(m) = file.mode {
                set.mode = m;
                set.mode_source = source.clone();
            }
            if let Some(agent) = &file.agent {
                set.push_grants(&Actor::Agent, &agent.grants, source, ctx);
                for p in &agent.ask.patterns {
                    set.ask_patterns.push((p.clone(), source.clone()));
                }
            }
            for (name, g) in &file.mcp {
                set.push_grants(&Actor::Mcp(name.clone()), g, source, ctx);
            }
            for (name, g) in &file.plugin {
                set.push_grants(&Actor::Plugin(name.clone()), g, source, ctx);
            }
            for p in &file.deny.fs_read {
                set.deny
                    .push((DenyRule::read(resolve_path_with(p, ctx)), source.clone()));
            }
            for p in &file.deny.fs_write {
                set.deny
                    .push((DenyRule::write(resolve_path_with(p, ctx)), source.clone()));
            }
            if file.deny.exec != ExecGrant::Unset
                || file.deny.net != NetGrant::Off
                || !file.deny.env.is_empty()
            {
                set.warnings.push(format!(
                    "{source}: [deny] wertet nur fs_read und fs_write aus; exec/net/env werden ignoriert"
                ));
            }
        }

        if let Some(m) = mode_override {
            set.mode = m;
            set.mode_source = Source::Cli;
        }
        set
    }

    fn push_entry(&mut self, actor: &Actor, cap: Capability, raw: &str, source: &Source) {
        if self
            .entries
            .iter()
            .any(|e| e.actor == *actor && e.cap == cap)
        {
            return; // erste Quelle gewinnt (Anzeige), Recht ist ohnehin identisch
        }
        self.entries.push(GrantEntry {
            actor: actor.clone(),
            cap,
            raw: raw.to_string(),
            source: source.clone(),
        });
    }

    fn push_grants(&mut self, actor: &Actor, g: &Grants, source: &Source, ctx: &ResolveCtx) {
        for raw in &g.fs_read {
            if raw == "system" {
                for s in system_read_paths() {
                    self.push_entry(
                        actor,
                        Capability::FsRead {
                            prefix: PathBuf::from(s),
                        },
                        raw,
                        source,
                    );
                }
            } else {
                self.push_entry(
                    actor,
                    Capability::FsRead {
                        prefix: resolve_path_with(raw, ctx),
                    },
                    raw,
                    source,
                );
            }
        }
        for raw in &g.fs_write {
            self.push_entry(
                actor,
                Capability::FsWrite {
                    prefix: resolve_path_with(raw, ctx),
                },
                raw,
                source,
            );
        }
        match &g.exec {
            ExecGrant::Unset => {}
            ExecGrant::System => {
                if !self.exec_open.contains(actor) {
                    self.exec_open.push(actor.clone());
                }
            }
            ExecGrant::Programs(progs) => {
                for p in progs {
                    self.push_entry(actor, Capability::Exec { program: p.clone() }, p, source);
                }
            }
        }
        match &g.net {
            NetGrant::Off => {}
            NetGrant::All => {
                self.push_entry(actor, Capability::Net { host: "*".into() }, "true", source)
            }
            NetGrant::Hosts(hosts) => {
                for h in hosts {
                    self.push_entry(actor, Capability::Net { host: h.clone() }, h, source);
                }
            }
        }
        for e in &g.env {
            self.push_entry(actor, Capability::Env { name: e.clone() }, e, source);
        }
    }

    /// Alle Verbote (ohne Herkunft).
    pub fn deny_rules(&self) -> Vec<DenyRule> {
        self.deny.iter().map(|(r, _)| r.clone()).collect()
    }

    /// Rohe Vereinigung der Gewährungen eines Akteurs (ohne Verbote).
    fn raw_policy_for(&self, actor: &Actor) -> Policy {
        let mut pol = Policy::default();
        for e in self.entries.iter().filter(|e| e.actor == *actor) {
            if !pol.granted.contains(&e.cap) {
                pol.granted.push(e.cap.clone());
            }
        }
        if self.exec_open.contains(actor) {
            pol.granted
                .retain(|c| !matches!(c, Capability::Exec { .. }));
        }
        pol
    }

    /// Effektive Policy eines Akteurs: Gewährungen minus Verbote (Grants unter einem Deny fallen
    /// weg; die Verbote stehen in `denied`, damit [`Policy::allows_path`] sie prüft).
    pub fn policy_for(&self, actor: &Actor) -> Policy {
        self.raw_policy_for(actor)
            .without_denied(&self.deny_rules())
            .0
    }

    /// Deny-Präfixe unterhalb einer Gewährung des Akteurs — für Kindprozesse nicht durchsetzbar.
    pub fn deny_overlaps(&self, actor: &Actor) -> Vec<DenyOverlap> {
        self.raw_policy_for(actor)
            .without_denied(&self.deny_rules())
            .1
    }

    /// Hat irgendeine Quelle diesem Akteur etwas gewährt (oder `exec = "system"` gesetzt)?
    pub fn has_entries(&self, actor: &Actor) -> bool {
        self.entries.iter().any(|e| e.actor == *actor) || self.exec_open.contains(actor)
    }

    /// Alle Akteure mit Einträgen, sortiert (Agent zuerst).
    pub fn actors(&self) -> Vec<Actor> {
        let mut out: Vec<Actor> = Vec::new();
        for e in &self.entries {
            if !out.contains(&e.actor) {
                out.push(e.actor.clone());
            }
        }
        for a in &self.exec_open {
            if !out.contains(a) {
                out.push(a.clone());
            }
        }
        out.sort();
        out
    }

    /// Erstes Rückfrage-Muster, das im Kommando vorkommt.
    pub fn ask_match(&self, command: &str) -> Option<&str> {
        self.ask_patterns
            .iter()
            .map(|(p, _)| p.as_str())
            .find(|p| !p.is_empty() && command.contains(p))
    }
}

/// Liest die Quellen in Reihenfolge (fehlende Dateien werden übersprungen) und führt sie mit
/// den Defaults zusammen.
pub fn load_policy_set(
    sources: &[PolicySource],
    defaults: &BuiltinDefaults,
    mode_override: Option<Mode>,
    ctx: &ResolveCtx,
) -> Result<PolicySet> {
    let mut files: Vec<(Source, PolicyFile)> = Vec::new();
    let mut seen: Vec<(PolicySource, bool)> = Vec::new();
    for src in sources {
        let text = match std::fs::read_to_string(&src.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                seen.push((src.clone(), false));
                continue;
            }
            Err(e) => {
                return Err(SeppError::Config(format!(
                    "policy {}: {e}",
                    src.path.display()
                )))
            }
        };
        let parsed = match src.kind {
            SourceKind::PolicyToml => Some(
                PolicyFile::parse(&text)
                    .map_err(|e| SeppError::Config(format!("{}: {e}", src.path.display())))?,
            ),
            SourceKind::SettingsToml => PolicyFile::from_settings_toml(&text)
                .map_err(|e| SeppError::Config(format!("{}: {e}", src.path.display())))?,
        };
        seen.push((src.clone(), true));
        if let Some(f) = parsed {
            files.push((Source::File(src.path.clone()), f));
        }
    }
    let mut set = PolicySet::merge(files, defaults, mode_override, ctx);
    set.sources = seen;
    Ok(set)
}

// ---------------------------------------------------------------------------------------------
// Entscheider: Guard
// ---------------------------------------------------------------------------------------------

/// Eine Aktion, die ein Akteur ausführen will.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    FsRead(PathBuf),
    FsWrite(PathBuf),
    Shell { command: String },
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::FsRead(p) => write!(f, "lesen {}", p.display()),
            Action::FsWrite(p) => write!(f, "schreiben {}", p.display()),
            Action::Shell { command } => {
                let short: String = command.chars().take(120).collect();
                if short.len() < command.len() {
                    write!(f, "bash: {short}…")
                } else {
                    write!(f, "bash: {short}")
                }
            }
        }
    }
}

/// Ergebnis von [`Guard::decide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask { reason: String },
    Deny { reason: String },
}

/// Rückfrage an den Menschen (Phase 2: TUI-Dialog).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub actor: Actor,
    pub action: String,
    pub reason: String,
}

/// Antwort des Menschen auf eine Rückfrage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionAnswer {
    /// Nur dieser Aufruf.
    Once,
    /// Für den Rest der Sitzung.
    Session,
    /// Dauerhaft (Sitzung + Persistenz-Anfrage für die Policy-Datei).
    Always,
    No,
}

/// Wer die Rückfrage stellt (TUI-Dialog, RPC-Ereignis, …).
#[async_trait::async_trait]
pub trait PermissionPrompter: Send + Sync {
    async fn ask(&self, req: PermissionRequest) -> PermissionAnswer;
}

/// Eine protokollierte Entscheidung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    pub actor: Actor,
    pub action: String,
    pub decision: String,
    pub detail: Option<String>,
}

/// Ergebnis einer erfolgreichen Autorisierung: zusätzliche, nur für diesen Aufruf geltende
/// Rechte (Antwort „einmal").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Authorization {
    pub extra: Vec<Capability>,
}

/// Der Entscheider. Hält das Regelwerk, den Modus, den Sandbox-Adapter, die Sitzungs-Gewährungen
/// und das Audit. `Send + Sync`, wird als `Arc<Guard>` an die Tools gegeben.
pub struct Guard {
    set: PolicySet,
    mode: Mode,
    /// Rückfrage-Kanal — wird erst gesetzt, wenn das Frontend steht (die TUI erzeugt ihren
    /// Kanal nach dem Guard-Bau), deshalb innere Mutability statt Builder-Feld.
    prompter: Mutex<Option<Arc<dyn PermissionPrompter>>>,
    session_grants: Mutex<Vec<Capability>>,
    /// Shell-Kommandos, die der Mensch für diese Sitzung freigegeben hat (Rückfrage-Muster).
    session_shell_ok: Mutex<Vec<String>>,
    sandbox: Box<dyn Sandbox>,
    audit: Mutex<Vec<AuditEvent>>,
    /// Meldungen fürs Frontend (z. B. „dauerhaft in policy.toml eingetragen").
    notices: Mutex<Vec<String>>,
    /// Policy-Datei für Hinweise und für „dauerhaft erlauben".
    policy_file: Option<PathBuf>,
}

impl fmt::Debug for Guard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Guard")
            .field("mode", &self.mode)
            .field("entries", &self.set.entries.len())
            .finish()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Guard {
    /// Neuer Guard; der Modus kommt aus dem [`PolicySet`].
    pub fn new(set: PolicySet, sandbox: Box<dyn Sandbox>) -> Self {
        Guard {
            mode: set.mode,
            set,
            prompter: Mutex::new(None),
            session_grants: Mutex::new(Vec::new()),
            session_shell_ok: Mutex::new(Vec::new()),
            sandbox,
            audit: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            policy_file: None,
        }
    }

    /// Rückfrage-Kanal. Ohne Prompter wird `Ask` zu `Deny` mit Hinweis.
    pub fn with_prompter(self, prompter: Arc<dyn PermissionPrompter>) -> Self {
        self.set_prompter(prompter);
        self
    }

    /// Setzt den Rückfrage-Kanal nachträglich — die TUI erzeugt ihren Kanal erst beim Start,
    /// der Guard steckt zu dem Zeitpunkt schon in den Tools.
    pub fn set_prompter(&self, prompter: Arc<dyn PermissionPrompter>) {
        *lock(&self.prompter) = Some(prompter);
    }

    /// Gibt es einen Rückfrage-Kanal? (Startup-Hinweis im Modus `ask`.)
    pub fn has_prompter(&self) -> bool {
        lock(&self.prompter).is_some()
    }

    /// Policy-Datei: wird in Verweigerungsmeldungen genannt und bei „dauerhaft erlauben"
    /// beschrieben.
    pub fn with_policy_file(mut self, path: PathBuf) -> Self {
        self.policy_file = Some(path);
        self
    }

    /// Pfad der Policy-Datei, in die „dauerhaft" schreibt.
    pub fn policy_file(&self) -> Option<&Path> {
        self.policy_file.as_deref()
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn policy_set(&self) -> &PolicySet {
        &self.set
    }

    pub fn sandbox(&self) -> &dyn Sandbox {
        self.sandbox.as_ref()
    }

    /// Effektive Policy des Akteurs inklusive Sitzungs-Gewährungen (nur für den Agenten).
    fn effective_policy(&self, actor: &Actor) -> Policy {
        let mut pol = self.set.policy_for(actor);
        if *actor == Actor::Agent {
            let session = lock(&self.session_grants).clone();
            for c in session {
                if !pol.granted.contains(&c) {
                    pol.granted.push(c);
                }
            }
        }
        pol
    }

    /// Reine Entscheidung nach der Modus-Tabelle (kein I/O, keine Rückfrage).
    pub fn decide(&self, actor: &Actor, action: &Action) -> Decision {
        if self.mode == Mode::Yolo {
            return Decision::Allow;
        }
        match action {
            Action::Shell { command } => {
                if self.mode == Mode::Ask {
                    if let Some(p) = self.set.ask_match(command) {
                        // Für diese Sitzung bereits freigegeben? Dann nicht erneut fragen.
                        if lock(&self.session_shell_ok).iter().any(|c| c == command) {
                            return Decision::Allow;
                        }
                        return Decision::Ask {
                            reason: format!("Kommando enthält Rückfrage-Muster '{p}'"),
                        };
                    }
                }
                Decision::Allow
            }
            Action::FsRead(p) | Action::FsWrite(p) => {
                let write = matches!(action, Action::FsWrite(_));
                let path = canonicalize_lenient(p);
                if let Some(rule) = self
                    .set
                    .deny_rules()
                    .into_iter()
                    .find(|r| r.blocks(&path, write))
                {
                    return Decision::Deny {
                        reason: format!("verweigert durch [deny] {}", rule.prefix.display()),
                    };
                }
                if self.effective_policy(actor).allows_path(&path, write) {
                    return Decision::Allow;
                }
                let reason = format!("{} liegt außerhalb der Policy für {actor}", path.display());
                match self.mode {
                    Mode::Auto => Decision::Deny { reason },
                    _ => Decision::Ask { reason },
                }
            }
        }
    }

    fn record(&self, actor: &Actor, action: &Action, decision: &str, detail: Option<String>) {
        lock(&self.audit).push(AuditEvent {
            actor: actor.clone(),
            action: action.to_string(),
            decision: decision.to_string(),
            detail,
        });
    }

    fn hint_text(&self, actor: &Actor, action: &Action) -> String {
        let file = self
            .policy_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| ".sepp/policy.toml".to_string());
        match action {
            Action::FsRead(p) => format!(
                "Freigeben: `sepp policy allow {actor} fs_read {}` oder Eintrag in {file} unter [{actor}]",
                p.display()
            ),
            Action::FsWrite(p) => format!(
                "Freigeben: `sepp policy allow {actor} fs_write {}` oder Eintrag in {file} unter [{actor}]",
                p.display()
            ),
            Action::Shell { .. } => {
                format!("Rechte in {file} unter [{actor}] anpassen oder mit --mode yolo starten")
            }
        }
    }

    fn deny_message(&self, actor: &Actor, action: &Action, reason: &str) -> String {
        format!(
            "{action} verweigert: {reason}. {}",
            self.hint_text(actor, action)
        )
    }

    /// Autorisiert eine Aktion: `Allow` → Ok, `Deny` → Fehler, `Ask` → Rückfrage über den Prompter
    /// (ohne Prompter: Fehler mit Hinweis). Locks werden nie über ein `.await` gehalten.
    pub async fn authorize(&self, actor: &Actor, action: Action) -> Result<Authorization> {
        match self.decide(actor, &action) {
            Decision::Allow => {
                self.record(actor, &action, "allow", None);
                Ok(Authorization::default())
            }
            Decision::Deny { reason } => {
                self.record(actor, &action, "deny", Some(reason.clone()));
                Err(SeppError::CapabilityDenied(
                    self.deny_message(actor, &action, &reason),
                ))
            }
            Decision::Ask { reason } => {
                // Prompter klonen und den Lock SOFORT freigeben — nie über ein `.await` halten
                // (sonst ist die Future nicht `Send` und der Tool-JoinSet nimmt sie nicht an).
                let prompter = lock(&self.prompter).clone();
                let Some(prompter) = prompter else {
                    self.record(
                        actor,
                        &action,
                        "deny (kein Rückfrage-Dialog)",
                        Some(reason.clone()),
                    );
                    return Err(SeppError::CapabilityDenied(format!(
                        "{action} verweigert: {reason}. Nachfrage-Dialog folgt (Phase 2); {}",
                        self.hint_text(actor, &action)
                    )));
                };
                let req = PermissionRequest {
                    actor: actor.clone(),
                    action: action.to_string(),
                    reason: reason.clone(),
                };
                // Kein Lock über dem await.
                let answer = prompter.ask(req).await;
                let cap = cap_for(&action);
                match answer {
                    PermissionAnswer::Once => {
                        self.record(actor, &action, "allow (einmal)", Some(reason));
                        Ok(Authorization {
                            extra: cap.into_iter().collect(),
                        })
                    }
                    PermissionAnswer::Session => {
                        self.grant_for_session(&action, cap);
                        self.record(actor, &action, "allow (Sitzung)", Some(reason));
                        Ok(Authorization::default())
                    }
                    PermissionAnswer::Always => {
                        self.grant_for_session(&action, cap);
                        let detail = self.persist(actor, &action);
                        self.record(actor, &action, "allow (dauerhaft)", Some(reason));
                        if let Some(msg) = detail {
                            lock(&self.notices).push(msg);
                        }
                        Ok(Authorization::default())
                    }
                    PermissionAnswer::No => {
                        self.record(actor, &action, "deny (Nutzer)", Some(reason.clone()));
                        Err(SeppError::CapabilityDenied(
                            self.deny_message(actor, &action, &reason),
                        ))
                    }
                }
            }
        }
    }

    /// Effektive Policy für einen bash-Spawn: Agent-Gewährungen ∪ Sitzung ∪ `extra`, minus
    /// Verbote. Wird an [`Guard::prepare_process`] übergeben.
    pub fn agent_spawn_policy(&self, extra: &[Capability]) -> Policy {
        let mut pol = self.effective_policy(&Actor::Agent);
        for c in extra {
            if !pol.granted.contains(c) {
                pol.granted.push(c.clone());
            }
        }
        pol.without_denied(&self.set.deny_rules()).0
    }

    /// Sperrt einen Kindprozess gemäß `policy` ein (Env-Scrubbing + Landlock/Seatbelt).
    pub fn prepare_process(
        &self,
        cmd: &mut tokio::process::Command,
        policy: &Policy,
    ) -> Result<()> {
        self.sandbox.prepare(cmd, policy)
    }

    /// Merkt eine bejahte Rückfrage für den Rest der Sitzung vor (Pfad-Recht bzw. das exakte
    /// Shell-Kommando — beim Muster-Treffer gibt es kein Recht, das man gewähren könnte).
    fn grant_for_session(&self, action: &Action, cap: Option<Capability>) {
        match (action, cap) {
            (Action::Shell { command }, _) => {
                let mut ok = lock(&self.session_shell_ok);
                if !ok.iter().any(|c| c == command) {
                    ok.push(command.clone());
                }
            }
            (_, Some(c)) => {
                let mut grants = lock(&self.session_grants);
                if !grants.contains(&c) {
                    grants.push(c);
                }
            }
            (_, None) => {}
        }
    }

    /// Schreibt eine bejahte Rückfrage dauerhaft in die Policy-Datei. Liefert die Meldung fürs
    /// Frontend; ein Schreibfehler hebt die Zustimmung NICHT auf (die Sitzung gilt weiter).
    fn persist(&self, actor: &Actor, action: &Action) -> Option<String> {
        let file = self.policy_file.as_ref()?;
        let (right, val) = match action {
            Action::FsRead(p) => ("fs_read", canonicalize_lenient(p).display().to_string()),
            Action::FsWrite(p) => ("fs_write", canonicalize_lenient(p).display().to_string()),
            // Ein Rückfrage-Muster gewährt kein Recht — „dauerhaft" hieße hier, das Muster aus
            // der Datei zu nehmen; das bleibt eine bewusste Handentscheidung.
            Action::Shell { .. } => {
                return Some(format!(
                "Für die Sitzung erlaubt. Dauerhaft: das Muster in {} unter [agent.ask] entfernen.",
                file.display()
            ))
            }
        };
        match crate::policy_edit::allow(file, actor, right, &val) {
            Ok(true) => Some(format!(
                "Dauerhaft erlaubt: {right} = \"{val}\" in {}",
                file.display()
            )),
            Ok(false) => Some(format!("Stand bereits in {}", file.display())),
            Err(e) => Some(format!(
                "Für die Sitzung erlaubt, aber Schreiben in {} schlug fehl: {e}",
                file.display()
            )),
        }
    }

    /// Holt Meldungen fürs Frontend ab (und leert sie).
    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.notices))
    }

    /// Holt alle bisher protokollierten Entscheidungen ab (und leert das Audit).
    pub fn drain_audit(&self) -> Vec<AuditEvent> {
        std::mem::take(&mut *lock(&self.audit))
    }

    /// Letzte protokollierte Entscheidung (für `ToolResult.details["guard"]`).
    pub fn last_audit(&self) -> Option<AuditEvent> {
        lock(&self.audit).last().cloned()
    }

    /// Kompakte JSON-Form eines Audit-Eintrags.
    pub fn audit_json(ev: &AuditEvent) -> serde_json::Value {
        serde_json::json!({
            "actor": ev.actor.to_string(),
            "action": ev.action,
            "decision": ev.decision,
            "detail": ev.detail,
        })
    }
}

/// Die Capability, die eine bejahte Rückfrage gewährt (Shell-Muster gewähren nichts).
fn cap_for(action: &Action) -> Option<Capability> {
    match action {
        Action::FsRead(p) => Some(Capability::FsRead {
            prefix: canonicalize_lenient(p),
        }),
        Action::FsWrite(p) => Some(Capability::FsWrite {
            prefix: canonicalize_lenient(p),
        }),
        Action::Shell { .. } => None,
    }
}

/// Hilfsfunktion für Frontends: liegt `path` (kanonisch) unter einem der Präfixe?
pub fn path_under(path: &Path, prefixes: &[PathBuf]) -> bool {
    prefixes.iter().any(|p| path.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NullSandbox;

    const EXAMPLE: &str = r#"
mode = "ask"

[agent]
fs_read  = ["./", "system", "~/.cargo/registry"]
fs_write = ["./", "$TMPDIR"]
exec     = "system"
net      = false
env      = ["PATH","HOME","LANG","LC_ALL","LC_CTYPE","TERM","TMPDIR"]

[agent.ask]
patterns = ["rm -rf", "git push --force", "git reset --hard"]

[mcp.git]
fs_read  = ["./"]
fs_write = ["./"]
exec     = ["git"]
env      = ["GIT_AUTHOR_NAME"]

[plugin.string-tools]
net      = ["api.example.com"]

[deny]
fs_read  = ["~/.ssh", "~/.aws", "~/.gnupg", "~/.sepp"]
"#;

    fn ctx() -> ResolveCtx {
        ResolveCtx {
            home: Some(PathBuf::from("/home/u")),
            cwd: PathBuf::from("/proj"),
            tmpdir: PathBuf::from("/tmp/t"),
        }
    }

    fn file(src: &str, f: PolicyFile) -> (Source, PolicyFile) {
        (Source::File(PathBuf::from(src)), f)
    }

    #[test]
    fn parses_full_example() {
        let f = PolicyFile::parse(EXAMPLE).unwrap();
        assert_eq!(f.mode, Some(Mode::Ask));
        let agent = f.agent.as_ref().unwrap();
        assert_eq!(agent.grants.fs_read.len(), 3);
        assert_eq!(agent.grants.exec, ExecGrant::System);
        assert_eq!(agent.grants.net, NetGrant::Off);
        assert_eq!(agent.grants.env.len(), 7);
        assert_eq!(agent.ask.patterns.len(), 3);
        let git = &f.mcp["git"];
        assert_eq!(git.exec, ExecGrant::Programs(vec!["git".into()]));
        assert_eq!(git.env, vec!["GIT_AUTHOR_NAME".to_string()]);
        assert_eq!(
            f.plugin["string-tools"].net,
            NetGrant::Hosts(vec!["api.example.com".into()])
        );
        assert_eq!(f.deny.fs_read.len(), 4);
        assert!(f.deny.fs_write.is_empty());
    }

    #[test]
    fn defaults_apply_without_files() {
        let defaults = BuiltinDefaults {
            extra_deny: vec![PathBuf::from("/etc/sepp")],
            default_mode: Mode::Auto,
        };
        let set = PolicySet::merge(vec![], &defaults, None, &ctx());
        assert_eq!(set.mode, Mode::Auto);
        assert_eq!(set.mode_source, Source::Builtin);
        let pol = set.policy_for(&Actor::Agent);
        assert!(pol.allows_path(Path::new("/proj/src/main.rs"), true));
        assert!(pol.allows_path(Path::new("/tmp/t/x"), true));
        assert!(pol.allows_path(Path::new("/usr/bin/ls"), false));
        assert!(!pol.allows_path(Path::new("/usr/bin/ls"), true));
        assert!(!pol.allows_path(Path::new("/home/u/notes"), false));
        assert!(!pol.net_allowed());
        assert_eq!(pol.exec_programs(), None);
        assert!(pol.allows(&Capability::Env {
            name: "PATH".into()
        }));
        // Eingebaute Verbote + extra_deny.
        let deny = set.deny_rules();
        assert!(deny.contains(&DenyRule::read("/home/u/.ssh")));
        assert!(deny.contains(&DenyRule::read("/home/u/.sepp")));
        assert!(deny.contains(&DenyRule::read("/etc/sepp")));
        assert!(set
            .entries
            .iter()
            .all(|e| e.source == Source::Builtin && e.actor == Actor::Agent));
        assert!(set.entries.iter().any(|e| e.raw == "system"
            && e.cap
                == Capability::FsRead {
                    prefix: "/usr".into()
                }));
    }

    #[test]
    fn merge_is_union_in_load_order_with_provenance() {
        let a = PolicyFile {
            agent: Some(AgentSection {
                grants: Grants {
                    fs_read: vec!["/data".into()],
                    ..Grants::default()
                },
                ..AgentSection::default()
            }),
            ..PolicyFile::default()
        };
        let b = PolicyFile {
            agent: Some(AgentSection {
                grants: Grants {
                    fs_read: vec!["/data".into(), "/other".into()],
                    net: NetGrant::All,
                    ..Grants::default()
                },
                ..AgentSection::default()
            }),
            ..PolicyFile::default()
        };
        let set = PolicySet::merge(
            vec![file("/g.toml", a), file("/p.toml", b)],
            &BuiltinDefaults::default(),
            None,
            &ctx(),
        );
        // Eingebaute Defaults bleiben erhalten, die Dateien erweitern sie.
        assert!(set
            .entries
            .iter()
            .any(|e| e.source == Source::Builtin && e.raw == "system"));
        let data = set
            .entries
            .iter()
            .find(|e| {
                e.cap
                    == Capability::FsRead {
                        prefix: "/data".into(),
                    }
            })
            .unwrap();
        assert_eq!(data.source, Source::File("/g.toml".into()));
        let other = set
            .entries
            .iter()
            .find(|e| {
                e.cap
                    == Capability::FsRead {
                        prefix: "/other".into(),
                    }
            })
            .unwrap();
        assert_eq!(other.source, Source::File("/p.toml".into()));
        let pol = set.policy_for(&Actor::Agent);
        assert!(pol.net_allowed());
        assert!(pol.allows_path(Path::new("/other/x"), false));
        assert!(set.has_entries(&Actor::Agent));
        assert!(!set.has_entries(&Actor::Mcp("git".into())));
    }

    #[test]
    fn deny_wins_over_any_grant_and_reports_overlap() {
        let f = PolicyFile {
            agent: Some(AgentSection {
                grants: Grants {
                    fs_read: vec!["~".into()],
                    fs_write: vec!["~/.ssh/authorized".into()],
                    ..Grants::default()
                },
                ..AgentSection::default()
            }),
            ..PolicyFile::default()
        };
        let set = PolicySet::merge(
            vec![file("/p.toml", f)],
            &BuiltinDefaults::default(),
            None,
            &ctx(),
        );
        let pol = set.policy_for(&Actor::Agent);
        assert!(!pol.allows_path(Path::new("/home/u/.ssh/id_ed25519"), false));
        assert!(!pol.allows_path(Path::new("/home/u/.ssh/authorized"), true));
        assert!(pol.allows_path(Path::new("/home/u/notes"), false));
        // Grant unter Deny ist weg, Deny unter Grant wird gemeldet.
        assert!(!pol.granted.contains(&Capability::FsWrite {
            prefix: "/home/u/.ssh/authorized".into()
        }));
        let overlaps = set.deny_overlaps(&Actor::Agent);
        assert!(overlaps
            .iter()
            .any(|o| o.grant == Path::new("/home/u") && o.deny == Path::new("/home/u/.ssh")));
    }

    #[test]
    fn mode_last_source_wins_then_cli_override() {
        let a = PolicyFile {
            mode: Some(Mode::Auto),
            ..PolicyFile::default()
        };
        let b = PolicyFile {
            mode: Some(Mode::Ask),
            ..PolicyFile::default()
        };
        let set = PolicySet::merge(
            vec![file("/a", a.clone()), file("/b", b.clone())],
            &BuiltinDefaults::default(),
            None,
            &ctx(),
        );
        assert_eq!(set.mode, Mode::Ask);
        assert_eq!(set.mode_source, Source::File("/b".into()));
        let set = PolicySet::merge(
            vec![file("/a", a), file("/b", b)],
            &BuiltinDefaults::default(),
            Some(Mode::Yolo),
            &ctx(),
        );
        assert_eq!(set.mode, Mode::Yolo);
        assert_eq!(set.mode_source, Source::Cli);
        // Ohne Quelle gilt der Frontend-Default.
        let set = PolicySet::merge(
            vec![],
            &BuiltinDefaults {
                default_mode: Mode::Auto,
                ..BuiltinDefaults::default()
            },
            None,
            &ctx(),
        );
        assert_eq!(set.mode, Mode::Auto);
        assert_eq!("ask".parse::<Mode>().unwrap(), Mode::Ask);
        assert!("egal".parse::<Mode>().is_err());
    }

    #[test]
    fn net_grant_forms() {
        let off = PolicyFile::parse("[agent]\nnet = false").unwrap();
        assert_eq!(off.agent.unwrap().grants.net, NetGrant::Off);
        let all = PolicyFile::parse("[agent]\nnet = true").unwrap();
        assert_eq!(all.agent.unwrap().grants.net, NetGrant::All);
        let hosts = PolicyFile::parse("[agent]\nnet = [\"a\", \"b\"]").unwrap();
        assert_eq!(
            hosts.agent.unwrap().grants.net,
            NetGrant::Hosts(vec!["a".into(), "b".into()])
        );
        let empty = PolicyFile::parse("[agent]\nnet = []").unwrap();
        assert_eq!(empty.agent.unwrap().grants.net, NetGrant::Off);
        // All → Wildcard-Recht, Hosts → je ein Recht.
        let pol = Grants {
            net: NetGrant::All,
            ..Grants::default()
        }
        .to_policy_with(&ctx());
        assert!(pol.allows(&Capability::Net {
            host: "irgendwo".into()
        }));
    }

    #[test]
    fn exec_grant_rejects_unknown_keyword() {
        assert!(PolicyFile::parse("[agent]\nexec = \"everything\"").is_err());
        let f = PolicyFile::parse("[agent]\nexec = [\"sh\", \"cargo\"]").unwrap();
        assert_eq!(
            f.agent.unwrap().grants.exec,
            ExecGrant::Programs(vec!["sh".into(), "cargo".into()])
        );
        let unset = PolicyFile::parse("[agent]\nfs_read = [\"./\"]").unwrap();
        assert_eq!(unset.agent.unwrap().grants.exec, ExecGrant::Unset);
    }

    #[test]
    fn exec_system_from_any_source_opens_exec() {
        let mut a = PolicyFile::default();
        a.mcp.insert(
            "git".into(),
            Grants {
                exec: ExecGrant::Programs(vec!["git".into()]),
                ..Grants::default()
            },
        );
        let set = PolicySet::merge(
            vec![file("/a", a.clone())],
            &BuiltinDefaults::default(),
            None,
            &ctx(),
        );
        assert_eq!(
            set.policy_for(&Actor::Mcp("git".into())).exec_programs(),
            Some(vec!["git".to_string()])
        );
        let mut b = PolicyFile::default();
        b.mcp.insert(
            "git".into(),
            Grants {
                exec: ExecGrant::System,
                ..Grants::default()
            },
        );
        let set = PolicySet::merge(
            vec![file("/a", a), file("/b", b)],
            &BuiltinDefaults::default(),
            None,
            &ctx(),
        );
        assert_eq!(
            set.policy_for(&Actor::Mcp("git".into())).exec_programs(),
            None
        );
    }

    #[test]
    fn system_keyword_and_tmpdir_expand() {
        let f = PolicyFile::parse("[agent]\nfs_read = [\"system\"]\nfs_write = [\"$TMPDIR/out\"]")
            .unwrap();
        let set = PolicySet::merge(
            vec![file("/p", f)],
            &BuiltinDefaults::default(),
            None,
            &ctx(),
        );
        for s in system_read_paths() {
            assert!(set.entries.iter().any(|e| e.raw == "system"
                && e.cap
                    == Capability::FsRead {
                        prefix: PathBuf::from(s)
                    }));
        }
        assert!(set.entries.iter().any(|e| e.cap
            == Capability::FsWrite {
                prefix: "/tmp/t/out".into()
            }));
    }

    #[test]
    fn from_settings_toml_ignores_mcp_and_missing_policy() {
        let without = r#"
[[mcp.servers]]
name = "git"
transport = "stdio"
command = ["uvx", "mcp-server-git"]
"#;
        assert_eq!(PolicyFile::from_settings_toml(without).unwrap(), None);
        let with = format!("{without}\n[policy]\nmode = \"auto\"\n[policy.agent]\nnet = true\n");
        let f = PolicyFile::from_settings_toml(&with).unwrap().unwrap();
        assert_eq!(f.mode, Some(Mode::Auto));
        assert_eq!(f.agent.unwrap().grants.net, NetGrant::All);
    }

    #[test]
    fn deny_with_non_fs_fields_emits_warning() {
        let f = PolicyFile::parse("[deny]\nfs_read = [\"/x\"]\nnet = true").unwrap();
        let set = PolicySet::merge(
            vec![file("/p", f)],
            &BuiltinDefaults::default(),
            None,
            &ctx(),
        );
        assert_eq!(set.warnings.len(), 1);
        assert!(set.warnings[0].contains("[deny]"));
        assert!(set.deny_rules().contains(&DenyRule::read("/x")));
    }

    #[test]
    fn load_policy_set_skips_missing_and_reads_both_kinds() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = tmp.path().join("settings.toml");
        std::fs::write(&settings, "[policy]\nmode = \"auto\"\n").unwrap();
        let project = tmp.path().join("policy.toml");
        std::fs::write(&project, "[agent]\nnet = true\n").unwrap();
        let missing = tmp.path().join("nope.toml");
        let sources = vec![
            PolicySource {
                path: settings.clone(),
                kind: SourceKind::SettingsToml,
            },
            PolicySource {
                path: missing.clone(),
                kind: SourceKind::PolicyToml,
            },
            PolicySource {
                path: project.clone(),
                kind: SourceKind::PolicyToml,
            },
        ];
        let set = load_policy_set(&sources, &BuiltinDefaults::default(), None, &ctx()).unwrap();
        assert_eq!(set.mode, Mode::Auto);
        assert!(set.policy_for(&Actor::Agent).net_allowed());
        assert_eq!(
            set.sources
                .iter()
                .map(|(_, found)| *found)
                .collect::<Vec<_>>(),
            vec![true, false, true]
        );
        // Kaputte Datei → Fehler mit Pfad.
        std::fs::write(&project, "[agent\n").unwrap();
        let err = load_policy_set(&sources, &BuiltinDefaults::default(), None, &ctx()).unwrap_err();
        assert!(err.to_string().contains("policy.toml"));
    }

    // ---- Guard -------------------------------------------------------------------------------

    struct Answering(PermissionAnswer);

    #[async_trait::async_trait]
    impl PermissionPrompter for Answering {
        async fn ask(&self, _req: PermissionRequest) -> PermissionAnswer {
            self.0
        }
    }

    /// Testumgebung: `root/{home,proj,tmp}` real angelegt (kanonische Pfade für die Pfadprüfung).
    struct Env {
        _tmp: tempfile::TempDir,
        root: PathBuf,
    }

    impl Env {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            for d in ["home", "home/.ssh", "proj", "tmp"] {
                std::fs::create_dir_all(root.join(d)).unwrap();
            }
            Env { _tmp: tmp, root }
        }
        fn ctx(&self) -> ResolveCtx {
            ResolveCtx {
                home: Some(self.root.join("home")),
                cwd: self.root.join("proj"),
                tmpdir: self.root.join("tmp"),
            }
        }
        fn set(&self, mode: Mode, agent: Grants, patterns: &[&str]) -> PolicySet {
            let f = PolicyFile {
                mode: Some(mode),
                agent: Some(AgentSection {
                    grants: agent,
                    ask: AskRules {
                        patterns: patterns.iter().map(|s| s.to_string()).collect(),
                    },
                }),
                ..PolicyFile::default()
            };
            PolicySet::merge(
                vec![file("/p.toml", f)],
                &BuiltinDefaults::default(),
                None,
                &self.ctx(),
            )
        }
        fn default_grants() -> Grants {
            Grants {
                fs_read: vec!["./".into(), "system".into()],
                fs_write: vec!["./".into(), "$TMPDIR".into()],
                ..Grants::default()
            }
        }
        fn guard(&self, mode: Mode, patterns: &[&str]) -> Guard {
            Guard::new(
                self.set(mode, Self::default_grants(), patterns),
                Box::new(NullSandbox),
            )
        }
    }

    #[tokio::test]
    async fn auto_allows_inside_denies_outside() {
        let env = Env::new();
        let g = env.guard(Mode::Auto, &["rm -rf"]);
        assert!(g
            .authorize(
                &Actor::Agent,
                Action::FsRead(env.root.join("proj/src/main.rs"))
            )
            .await
            .is_ok());
        assert!(g
            .authorize(&Actor::Agent, Action::FsWrite(env.root.join("tmp/x")))
            .await
            .is_ok());
        let err = g
            .authorize(
                &Actor::Agent,
                Action::FsWrite(env.root.join("home/.config/x")),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("verweigert"), "{msg}");
        assert!(msg.contains("sepp policy allow agent fs_write"), "{msg}");
        // Muster werden in auto ignoriert.
        assert_eq!(
            g.decide(
                &Actor::Agent,
                &Action::Shell {
                    command: "rm -rf /".into()
                }
            ),
            Decision::Allow
        );
    }

    #[tokio::test]
    async fn ask_without_prompter_denies_outside_and_pattern() {
        let env = Env::new();
        let g = env.guard(Mode::Ask, &["rm -rf"]);
        let err = g
            .authorize(&Actor::Agent, Action::FsRead(env.root.join("home/notes")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Nachfrage-Dialog folgt"), "{err}");
        let err = g
            .authorize(
                &Actor::Agent,
                Action::Shell {
                    command: "rm -rf build".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Rückfrage-Muster"), "{err}");
        // Innerhalb der Policy bleibt ask = allow.
        assert!(g
            .authorize(&Actor::Agent, Action::FsRead(env.root.join("proj/a")))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn deny_prefix_beats_grant_in_both_modes() {
        let env = Env::new();
        for mode in [Mode::Auto, Mode::Ask] {
            let set = env.set(
                mode,
                Grants {
                    fs_read: vec!["~".into()],
                    ..Grants::default()
                },
                &[],
            );
            let g = Guard::new(set, Box::new(NullSandbox))
                .with_prompter(Arc::new(Answering(PermissionAnswer::Once)));
            let err = g
                .authorize(&Actor::Agent, Action::FsRead(env.root.join("home/.ssh/id")))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("[deny]"), "{err}");
            assert!(g
                .authorize(&Actor::Agent, Action::FsRead(env.root.join("home/notes")))
                .await
                .is_ok());
        }
    }

    #[tokio::test]
    async fn symlink_into_denied_dir_is_denied() {
        let env = Env::new();
        let link = env.root.join("proj/link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(env.root.join("home/.ssh"), &link).unwrap();
        #[cfg(not(unix))]
        return;
        let g = env.guard(Mode::Auto, &[]);
        let err = g
            .authorize(&Actor::Agent, Action::FsRead(link.join("id_ed25519")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("[deny]"), "{err}");
    }

    #[tokio::test]
    async fn session_grant_extends_policy() {
        let env = Env::new();
        let g = Guard::new(
            env.set(Mode::Ask, Env::default_grants(), &[]),
            Box::new(NullSandbox),
        )
        .with_prompter(Arc::new(Answering(PermissionAnswer::Session)));
        let target = env.root.join("home/notes.txt");
        let auth = g
            .authorize(&Actor::Agent, Action::FsWrite(target.clone()))
            .await
            .unwrap();
        assert!(auth.extra.is_empty());
        assert_eq!(
            g.decide(&Actor::Agent, &Action::FsWrite(target.clone())),
            Decision::Allow
        );
        assert!(g.agent_spawn_policy(&[]).allows_path(&target, true));
        // „Sitzung" schreibt nichts in die Policy-Datei.
        assert!(g.take_notices().is_empty());
    }

    #[tokio::test]
    async fn once_grant_only_in_extra() {
        let env = Env::new();
        let g = Guard::new(
            env.set(Mode::Ask, Env::default_grants(), &[]),
            Box::new(NullSandbox),
        )
        .with_prompter(Arc::new(Answering(PermissionAnswer::Once)));
        let target = env.root.join("home/once.txt");
        let auth = g
            .authorize(&Actor::Agent, Action::FsWrite(target.clone()))
            .await
            .unwrap();
        assert_eq!(
            auth.extra,
            vec![Capability::FsWrite {
                prefix: target.clone()
            }]
        );
        assert!(matches!(
            g.decide(&Actor::Agent, &Action::FsWrite(target.clone())),
            Decision::Ask { .. }
        ));
        assert!(g.agent_spawn_policy(&auth.extra).allows_path(&target, true));
        assert!(!g.agent_spawn_policy(&[]).allows_path(&target, true));
    }

    #[tokio::test]
    async fn always_writes_policy_file_and_no_denies() {
        let env = Env::new();
        let policy_file = env.root.join("proj/.sepp/policy.toml");
        let g = Guard::new(
            env.set(Mode::Ask, Env::default_grants(), &[]),
            Box::new(NullSandbox),
        )
        .with_policy_file(policy_file.clone())
        .with_prompter(Arc::new(Answering(PermissionAnswer::Always)));
        let target = env.root.join("home/always.txt");
        g.authorize(&Actor::Agent, Action::FsWrite(target.clone()))
            .await
            .unwrap();
        // Datei geschrieben, Meldung fürs Frontend, Sitzung gilt sofort.
        let written = std::fs::read_to_string(&policy_file).unwrap();
        assert!(written.contains("[agent]"), "{written}");
        assert!(
            written.contains(&format!("{}", target.display())),
            "{written}"
        );
        let notices = g.take_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("Dauerhaft erlaubt"), "{notices:?}");
        assert_eq!(
            g.decide(&Actor::Agent, &Action::FsWrite(target.clone())),
            Decision::Allow
        );
        let g = Guard::new(
            env.set(Mode::Ask, Env::default_grants(), &[]),
            Box::new(NullSandbox),
        )
        .with_prompter(Arc::new(Answering(PermissionAnswer::No)));
        assert!(g
            .authorize(&Actor::Agent, Action::FsWrite(target))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn audit_records_every_authorize() {
        let env = Env::new();
        let g = env.guard(Mode::Auto, &[]);
        let _ = g
            .authorize(&Actor::Agent, Action::FsRead(env.root.join("proj/a")))
            .await;
        let _ = g
            .authorize(&Actor::Agent, Action::FsRead(env.root.join("home/b")))
            .await;
        let last = g.last_audit().unwrap();
        assert_eq!(last.decision, "deny");
        let events = g.drain_audit();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].decision, "allow");
        assert_eq!(Guard::audit_json(&events[1])["decision"], "deny");
        assert!(g.drain_audit().is_empty());
    }

    #[tokio::test]
    async fn write_implies_read_and_yolo_allows_everything() {
        let env = Env::new();
        let set = env.set(
            Mode::Auto,
            Grants {
                fs_write: vec!["./".into()],
                ..Grants::default()
            },
            &[],
        );
        let g = Guard::new(set, Box::new(NullSandbox));
        assert!(g
            .authorize(&Actor::Agent, Action::FsRead(env.root.join("proj/x")))
            .await
            .is_ok());
        let yolo = env.guard(Mode::Yolo, &["rm -rf"]);
        assert_eq!(
            yolo.decide(
                &Actor::Agent,
                &Action::FsRead(env.root.join("home/.ssh/id"))
            ),
            Decision::Allow
        );
        assert_eq!(yolo.mode(), Mode::Yolo);
    }

    #[test]
    fn action_display_truncates_long_commands() {
        let long = "x".repeat(300);
        let s = Action::Shell { command: long }.to_string();
        assert!(s.len() < 140);
        assert!(s.ends_with('…'));
        assert_eq!(Action::FsRead("/a".into()).to_string(), "lesen /a");
    }
}
