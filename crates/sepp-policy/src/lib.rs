//! `sepp-policy` — Capabilities, Policy-Auswertung und OS-Sandbox.
//!
//! Default ist **deny**: was nicht deklariert ist, gibt es nicht.
//! Eine [`Policy`] ist die Menge gewährter [`Capability`]s, gelesen aus einem [`Manifest`], aus
//! `[mcp.servers.capabilities]` oder aus der Policy-Datei ([`guard`]). Durchsetzung
//! out-of-process über [`Sandbox`] (Linux: Landlock, macOS: Seatbelt; sonst portabler Fallback ohne
//! Durchsetzung + Warnung); in-process über die Pfadprüfung des [`guard::Guard`].
//!
//! **Sepp Guard** (Modul [`guard`]): ein Regelwerk (`policy.toml`), ein Entscheider ([`guard::Guard`]),
//! ein Audit, mehrere Vollstrecker (Landlock/Seatbelt für Kindprozesse, wasmi-Linker-Gate für
//! WASM, Pfadprüfung für die eingebauten Tools).

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use sepp_core::{Result, SeppError};

pub mod guard;
pub mod policy_edit;
mod sandbox;
mod secrets;
pub use guard::{
    builtin_agent_grants, load_policy_set, Action, Actor, AgentSection, AskRules, AuditEvent,
    Authorization, BuiltinDefaults, Decision, ExecGrant, GrantEntry, Grants, Guard, Mode, NetGrant,
    PermissionAnswer, PermissionPrompter, PermissionRequest, PolicyFile, PolicySet, PolicySource,
    Source, SourceKind, BUILTIN_DENY,
};
#[cfg(target_os = "linux")]
pub use sandbox::LandlockSandbox;
pub use sandbox::{
    default_sandbox, kernel_capabilities, probe_sandbox, resolve_program, NullSandbox, Sandbox,
    SandboxCapabilities,
};
pub use secrets::{placeholder_names, GateRefusal, SecretBroker};

/// Ein atomares Recht.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    FsRead {
        prefix: PathBuf,
    },
    FsWrite {
        prefix: PathBuf,
    },
    /// Host exakt (`api.example.com`) oder Glob (`*.example.com`).
    Net {
        host: String,
    },
    Env {
        name: String,
    },
    Exec {
        program: String,
    },
}

/// Art einer Deny-Regel: `Read` sperrt Lesen **und** Schreiben, `Write` nur Schreiben.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenyKind {
    Read,
    Write,
}

/// Ein Verbot, das jede Gewährung schlägt (Präfix-Semantik wie bei `FsRead`/`FsWrite`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenyRule {
    pub prefix: PathBuf,
    pub kind: DenyKind,
}

impl DenyRule {
    pub fn read(prefix: impl Into<PathBuf>) -> Self {
        DenyRule {
            prefix: prefix.into(),
            kind: DenyKind::Read,
        }
    }
    pub fn write(prefix: impl Into<PathBuf>) -> Self {
        DenyRule {
            prefix: prefix.into(),
            kind: DenyKind::Write,
        }
    }

    /// Greift die Regel für einen Zugriff auf `path` (`write` = Schreibzugriff)?
    pub fn blocks(&self, path: &Path, write: bool) -> bool {
        path.starts_with(&self.prefix) && (write || self.kind == DenyKind::Read)
    }
}

/// Ein Deny-Präfix, der **unterhalb** einer Gewährung liegt: für Kindprozesse (Landlock ist
/// additiv) nicht durchsetzbar, für die In-Process-Pfadprüfung schon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenyOverlap {
    pub grant: PathBuf,
    pub deny: PathBuf,
}

/// Sperrt eine der Regeln den Zugriff auf `path`?
pub fn is_denied(rules: &[DenyRule], path: &Path, write: bool) -> bool {
    rules.iter().any(|r| r.blocks(path, write))
}

/// Die Menge gewährter Capabilities (plus die Verbote, die Adapter mit Deny-Regeln — Seatbelt —
/// zusätzlich eintragen; Landlock kann sie nicht ausdrücken, siehe [`Policy::without_denied`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub granted: Vec<Capability>,
    #[serde(default)]
    pub denied: Vec<DenyRule>,
}

impl Policy {
    pub fn new(granted: Vec<Capability>) -> Self {
        Policy {
            granted,
            denied: Vec::new(),
        }
    }

    /// Erlaubt die Policy das angefragte Recht? Default deny.
    pub fn allows(&self, requested: &Capability) -> bool {
        self.granted.iter().any(|g| covers(g, requested))
    }

    /// Erlaubt die Policy den Datei-Zugriff? Schreibrecht impliziert Leserecht auf demselben
    /// Pfad (wie in der Sandbox, wo Schreibpräfixe auch lesbar sind). Verbote werden geprüft.
    pub fn allows_path(&self, path: &Path, write: bool) -> bool {
        if is_denied(&self.denied, path, write) {
            return false;
        }
        self.granted.iter().any(|g| match g {
            Capability::FsWrite { prefix } => path.starts_with(prefix),
            Capability::FsRead { prefix } if !write => path.starts_with(prefix),
            _ => false,
        })
    }

    /// Vereinigung (Reihenfolge erhalten, Duplikate entfernt).
    pub fn union(&self, other: &Policy) -> Policy {
        let mut out = self.clone();
        for c in &other.granted {
            if !out.granted.contains(c) {
                out.granted.push(c.clone());
            }
        }
        for d in &other.denied {
            if !out.denied.contains(d) {
                out.denied.push(d.clone());
            }
        }
        out
    }

    /// Schnitt von Gewährung (`self`) und Anfrage (`request`): je Paar bleibt das engere Recht.
    /// Ein Recht, das keine Seite deckt, fällt weg (Default deny).
    pub fn intersect(&self, request: &Policy) -> Policy {
        let mut out = Policy::default();
        for g in &self.granted {
            for r in &request.granted {
                let keep = if covers(g, r) {
                    Some(r)
                } else if covers(r, g) {
                    Some(g)
                } else {
                    None
                };
                if let Some(c) = keep {
                    if !out.granted.contains(c) {
                        out.granted.push(c.clone());
                    }
                }
            }
        }
        out.denied = self.denied.clone();
        for d in &request.denied {
            if !out.denied.contains(d) {
                out.denied.push(d.clone());
            }
        }
        out
    }

    /// Ist irgendein Netzzugriff gewährt (`*`, Host oder Glob)?
    pub fn net_allowed(&self) -> bool {
        self.granted
            .iter()
            .any(|c| matches!(c, Capability::Net { .. }))
    }

    /// Exec-Allowlist: `None` = unbeschränkt (keine `Exec`-Einträge), sonst die Programme.
    pub fn exec_programs(&self) -> Option<Vec<String>> {
        let progs: Vec<String> = self
            .granted
            .iter()
            .filter_map(|c| match c {
                Capability::Exec { program } => Some(program.clone()),
                _ => None,
            })
            .collect();
        if progs.is_empty() {
            None
        } else {
            Some(progs)
        }
    }

    /// Wendet Verbote auf die Gewährungen an: Grants, die **unter** einem Deny-Präfix liegen,
    /// fallen weg; Deny-Präfixe **unter** einem Grant werden als [`DenyOverlap`] gemeldet (für
    /// Kindprozesse nicht durchsetzbar). Die Regeln landen in `denied` (für Adapter, die Verbote
    /// ausdrücken können, und für [`Policy::allows_path`]).
    pub fn without_denied(&self, rules: &[DenyRule]) -> (Policy, Vec<DenyOverlap>) {
        let mut out = Policy::default();
        let mut overlaps = Vec::new();
        for c in &self.granted {
            let (prefix, write) = match c {
                Capability::FsRead { prefix } => (prefix, false),
                Capability::FsWrite { prefix } => (prefix, true),
                _ => {
                    out.granted.push(c.clone());
                    continue;
                }
            };
            if is_denied(rules, prefix, write) {
                continue;
            }
            for r in rules {
                let relevant = write || r.kind == DenyKind::Read;
                if relevant && r.prefix.starts_with(prefix) && r.prefix != *prefix {
                    let o = DenyOverlap {
                        grant: prefix.clone(),
                        deny: r.prefix.clone(),
                    };
                    if !overlaps.contains(&o) {
                        overlaps.push(o);
                    }
                }
            }
            out.granted.push(c.clone());
        }
        out.denied = self.denied.clone();
        for r in rules {
            if !out.denied.contains(r) {
                out.denied.push(r.clone());
            }
        }
        (out, overlaps)
    }

    /// Lese-Pfad-Präfixe (für die Sandbox).
    pub fn fs_read_prefixes(&self) -> Vec<PathBuf> {
        self.granted
            .iter()
            .filter_map(|c| match c {
                Capability::FsRead { prefix } => Some(prefix.clone()),
                _ => None,
            })
            .collect()
    }

    /// Schreib-Pfad-Präfixe (für die Sandbox).
    pub fn fs_write_prefixes(&self) -> Vec<PathBuf> {
        self.granted
            .iter()
            .filter_map(|c| match c {
                Capability::FsWrite { prefix } => Some(prefix.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Deckt das gewährte Recht `g` das angefragte `r`?
///
/// Ein Schreibrecht deckt die Leseanfrage auf demselben Präfix — nicht aus Bequemlichkeit,
/// sondern weil die Vollstrecker es ohnehin so machen und gar nicht anders können: Landlock
/// trägt jeden Schreibpfad zusätzlich in die Leseliste ein (siehe `sandbox.rs`), Seatbelt
/// schreibt `(allow file-read* file-write* …)`. Eine Policy „schreiben ja, lesen nein" wäre für
/// Kindprozesse nicht durchsetzbar, und wer eine Datei überschreiben darf, erfährt ihren Inhalt
/// ohnehin. [`Policy::allows_path`] liest es seit jeher so. Die Gegenrichtung bleibt streng:
/// Ein Leserecht deckt keine Schreibanfrage.
fn covers(g: &Capability, r: &Capability) -> bool {
    use Capability::*;
    match (g, r) {
        (FsRead { prefix: gp }, FsRead { prefix: rp }) => rp.starts_with(gp),
        (FsWrite { prefix: gp }, FsWrite { prefix: rp }) => rp.starts_with(gp),
        (FsWrite { prefix: gp }, FsRead { prefix: rp }) => rp.starts_with(gp),
        (Net { host: gh }, Net { host: rh }) => host_matches(gh, rh),
        (Env { name: gn }, Env { name: rn }) => gn == rn,
        (Exec { program: gp }, Exec { program: rp }) => gp == rp,
        _ => false,
    }
}

/// `*` matcht jeden Host; `*.example.com` matcht Subdomains (`api.example.com`), nicht den
/// Apex; sonst exakt.
fn host_matches(pattern: &str, host: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.strip_prefix("*.") {
        Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(&format!(".{suffix}")),
        None => pattern == host,
    }
}

/// Host einer http(s)-URL — ohne Schema, Userinfo, Port und Pfad.
///
/// Reicht für das Net-Gate: [`Capability::Net`] kennt nur Hostnamen (exakt oder `*.suffix`).
/// Andere Schemata liefern `None` — der Aufrufer lehnt sie ab. Genutzt vom MCP-Client
/// (Secret-Header) und vom WASM-Host (`host_http`), deshalb hier und nicht in einem von beiden.
pub fn url_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // Userinfo abschneiden (`user:pass@host`).
    let authority = authority.rsplit('@').next()?;
    let host = if let Some(v6) = authority.strip_prefix('[') {
        // IPv6-Literal: alles bis zur schließenden Klammer.
        v6.split(']').next()?
    } else {
        authority.split(':').next()?
    };
    (!host.is_empty()).then_some(host)
}

/// Vorgabe für [`Manifest::abi`]: ein Manifest ohne Angabe meint Version 1.
fn abi_v1() -> u32 {
    1
}

/// Manifest einer code-führenden Erweiterung (`manifest.toml`).
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub name: String,
    /// Version des Plugin-Protokolls, gegen das dieses Modul gebaut wurde. Der Host lehnt ab,
    /// was er nicht kennt — ohne diese Angabe bräche jede spätere Protokolländerung stumm
    /// jedes vorhandene Plugin. Fehlend = 1.
    #[serde(default = "abi_v1")]
    pub abi: u32,
    #[serde(default)]
    pub version: Option<String>,
    /// `"wasm"` | `"mcp"`.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Nur `kind = "wasm"`.
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Ressourcen-Limits (`[limits]`; fehlend = konservative Defaults, nie „unbegrenzt").
    #[serde(default)]
    pub limits: Limits,
    /// Alles, was der Host nicht kennt. Wird gelesen, ignoriert und **gemeldet**: Ein Tippfehler
    /// wie `capabilites` bliebe sonst stumm und kostet den Autor eine lange Suche. Striktes
    /// Ablehnen wäre die Alternative, würde aber jedes neuere Paket auf einem älteren sepp
    /// scheitern lassen.
    #[serde(flatten)]
    pub unknown: std::collections::BTreeMap<String, toml::Value>,
}

/// Deklarierte Capabilities (Manifest- bzw. `[mcp.servers.capabilities]`-Form).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub fs_read: Vec<String>,
    #[serde(default)]
    pub fs_write: Vec<String>,
    #[serde(default)]
    pub net: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub exec: Vec<String>,
}

impl Capabilities {
    /// Ist nichts angefordert? Genutzt, um eine veraltete `[mcp.servers.capabilities]`-Angabe
    /// zu erkennen und zu melden.
    pub fn is_empty(&self) -> bool {
        self.fs_read.is_empty()
            && self.fs_write.is_empty()
            && self.net.is_empty()
            && self.env.is_empty()
            && self.exec.is_empty()
    }
}

impl Capabilities {
    /// Baut eine [`Policy`]; Pfade werden (`~`/relativ aufgelöst und) kanonisiert.
    pub fn to_policy(&self) -> Policy {
        self.to_policy_with(&ResolveCtx::from_env())
    }

    /// Wie [`Capabilities::to_policy`], mit explizitem Auflösungskontext (testbar ohne Env).
    pub fn to_policy_with(&self, ctx: &ResolveCtx) -> Policy {
        let mut granted = Vec::new();
        for p in &self.fs_read {
            granted.push(Capability::FsRead {
                prefix: resolve_path_with(p, ctx),
            });
        }
        for p in &self.fs_write {
            granted.push(Capability::FsWrite {
                prefix: resolve_path_with(p, ctx),
            });
        }
        for h in &self.net {
            granted.push(Capability::Net { host: h.clone() });
        }
        for e in &self.env {
            granted.push(Capability::Env { name: e.clone() });
        }
        for x in &self.exec {
            granted.push(Capability::Exec { program: x.clone() });
        }
        Policy::new(granted)
    }
}

/// Ressourcen-Limits einer code-führenden Erweiterung (`[limits]` im Manifest).
///
/// Konzeptuell dieselbe Logik wie [`Capability`], nur für **Verbrauch** (CPU, Speicher, Wanduhr)
/// statt für Zugriff: kein deklariertes Limit heißt konservativer Default, nicht „unbegrenzt".
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Maximale lineare WASM-Speichergröße in 64-KiB-Pages (Default: 256 = 16 MiB).
    pub max_memory_pages: u32,
    /// Wanduhr-Budget eines Tool-Aufrufs in Millisekunden. `0` heißt: beliebig lange laufen
    /// dürfen — aber weiterhin an jedem Yield-Punkt unterbrechbar (Default: 30 000).
    pub max_wall_time_ms: u64,
    /// Instruktionen pro Zeitscheibe — das Yield-Intervall des Fuel-Slicings (Default: 1 000 000).
    pub fuel_slice: u64,
    /// HTTP-Anfragen über `host_http` je Werkzeugaufruf (Default: 16). Die Instanz lebt nur einen
    /// Aufruf lang; mehr Anfragen sind fast immer eine Schleife, die außer Kontrolle geraten ist.
    pub max_http_requests: u32,
    /// Größte Antwort, die `host_http` ans Modul reicht, in Bytes (Default: 4 MiB, höchstens
    /// [`MAX_HTTP_RESPONSE_BYTES`]). Wird **vor** dem Puffern geprüft (Content-Length) und beim
    /// Streamen gezählt. Binäre Antworten kommen base64-kodiert an und wachsen um 4/3 — nahe der
    /// Obergrenze laufen sie in den Ergebnisdeckel des Hosts („Ergebnis zu groß").
    pub max_http_response_bytes: u64,
    /// Zeitbudget je HTTP-Anfrage in Millisekunden (Default: 10 000). Wird zusätzlich auf die
    /// verbleibende Wanduhr des Aufrufs (`max_wall_time_ms`) gekappt; während eine Anfrage
    /// läuft, tickt die Wanduhr weiter.
    pub http_timeout_ms: u64,
}

/// Obergrenze für [`Limits::max_http_response_bytes`] — entspricht dem Ergebnisdeckel des
/// WASM-Hosts (`MAX_PLUGIN_BYTES`, 16 MiB); ein Test in `sepp-wasm` hält beide gleich.
pub const MAX_HTTP_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_memory_pages: 256,
            max_wall_time_ms: 30_000,
            fuel_slice: 1_000_000,
            max_http_requests: 16,
            max_http_response_bytes: 4 * 1024 * 1024,
            http_timeout_ms: 10_000,
        }
    }
}

impl Limits {
    /// Speicherlimit in Bytes (Pages × 64 KiB).
    pub fn max_memory_bytes(&self) -> usize {
        self.max_memory_pages as usize * 64 * 1024
    }

    /// Weist unplausible Werte zurück — lieber gar nicht laden als unkontrolliert laufen.
    pub fn validate(&self) -> Result<()> {
        if self.fuel_slice == 0 {
            return Err(SeppError::Config(
                "manifest [limits]: fuel_slice muss > 0 sein".into(),
            ));
        }
        if self.max_memory_pages == 0 || self.max_memory_pages > 65_536 {
            return Err(SeppError::Config(format!(
                "manifest [limits]: max_memory_pages muss in 1..=65536 liegen (ist {})",
                self.max_memory_pages
            )));
        }
        if self.max_http_requests == 0 {
            return Err(SeppError::Config(
                "manifest [limits]: max_http_requests muss > 0 sein (ohne Netz: kein net-Recht \
                 anfordern)"
                    .into(),
            ));
        }
        if self.max_http_response_bytes == 0
            || self.max_http_response_bytes > MAX_HTTP_RESPONSE_BYTES
        {
            return Err(SeppError::Config(format!(
                "manifest [limits]: max_http_response_bytes muss in 1..={MAX_HTTP_RESPONSE_BYTES} \
                 liegen (ist {})",
                self.max_http_response_bytes
            )));
        }
        if self.http_timeout_ms == 0 {
            return Err(SeppError::Config(
                "manifest [limits]: http_timeout_ms muss > 0 sein".into(),
            ));
        }
        Ok(())
    }
}

impl Manifest {
    pub fn parse(toml_str: &str) -> Result<Manifest> {
        let m: Manifest =
            toml::from_str(toml_str).map_err(|e| SeppError::Config(format!("manifest: {e}")))?;
        m.limits.validate()?;
        Ok(m)
    }

    pub fn from_file(path: &Path) -> Result<Manifest> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| SeppError::Config(format!("manifest {}: {e}", path.display())))?;
        Self::parse(&text)
    }

    pub fn policy(&self) -> Policy {
        self.capabilities.to_policy()
    }

    /// Namen der Felder, die der Host nicht kennt — sortiert, für eine stabile Meldung.
    pub fn unknown_keys(&self) -> Vec<&str> {
        self.unknown.keys().map(String::as_str).collect()
    }
}

/// Kontext der Pfadauflösung: Home (`~`), Arbeitsverzeichnis (relative Pfade) und `$TMPDIR`.
/// Explizit übergebbar, damit Auflösung ohne Env-Mutation testbar bleibt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveCtx {
    pub home: Option<PathBuf>,
    pub cwd: PathBuf,
    pub tmpdir: PathBuf,
}

impl ResolveCtx {
    /// Aus der Prozess-Umgebung: `HOME`, `current_dir()`, `TMPDIR` (Fallback `/tmp`).
    pub fn from_env() -> Self {
        ResolveCtx {
            home: std::env::var_os("HOME").map(PathBuf::from),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            tmpdir: std::env::var_os("TMPDIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp")),
        }
    }
}

/// Systempfade, die ein Kindprozess zum Starten lesen (und ausführen) darf; gemeinsam genutzt
/// von Landlock, Seatbelt und dem Schlüsselwort `"system"` in der Policy-Datei.
pub fn system_read_paths() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &[
            "/usr",
            "/bin",
            "/sbin",
            "/System",
            "/Library",
            "/private/etc",
            "/private/var/db/dyld",
            "/opt",
            "/Applications",
        ]
    }
    #[cfg(not(target_os = "macos"))]
    {
        &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/proc"]
    }
}

/// Löst `~` (Home), `$TMPDIR` und relative `./`-Pfade (gegen cwd) auf und kanonisiert best-effort.
pub fn resolve_path(p: &str) -> PathBuf {
    resolve_path_with(p, &ResolveCtx::from_env())
}

/// Wie [`resolve_path`], mit explizitem Kontext.
pub fn resolve_path_with(p: &str, ctx: &ResolveCtx) -> PathBuf {
    let expanded: PathBuf = if let Some(rest) = p.strip_prefix('~') {
        match &ctx.home {
            Some(home) => home.join(rest.trim_start_matches('/')),
            None => PathBuf::from(p),
        }
    } else if let Some(rest) = p.strip_prefix("$TMPDIR") {
        ctx.tmpdir.join(rest.trim_start_matches('/'))
    } else {
        PathBuf::from(p)
    };
    let abs = if expanded.is_absolute() {
        expanded
    } else {
        ctx.cwd.join(&expanded)
    };
    abs.canonicalize().unwrap_or(abs)
}

/// Kanonisiert auch Pfade, die (noch) nicht existieren: der längste existierende Vorfahr wird
/// kanonisiert (Symlinks aufgelöst), der Rest lexikalisch angehängt (`.` entfernt, `..` gepoppt).
/// Damit kann `./link-nach-ssh/neu` nicht als „im Projekt" durchgehen.
pub fn canonicalize_lenient(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    if let Ok(c) = abs.canonicalize() {
        return c;
    }
    // Längsten existierenden (kanonisierbaren) Präfix komponentenweise suchen …
    let comps: Vec<Component> = abs.components().collect();
    let mut cur = PathBuf::new();
    let mut best: Option<(PathBuf, usize)> = None;
    for (i, c) in comps.iter().enumerate() {
        cur.push(c.as_os_str());
        match cur.canonicalize() {
            Ok(canon) => best = Some((canon, i + 1)),
            Err(_) => break,
        }
    }
    let (mut out, start) = best.unwrap_or_else(|| (PathBuf::from("/"), 0));
    // … und den Rest lexikalisch anhängen.
    for c in &comps[start..] {
        match c {
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn url_host_strips_scheme_userinfo_port_and_path() {
        assert_eq!(
            url_host("https://api.example.com/mcp"),
            Some("api.example.com")
        );
        assert_eq!(
            url_host("http://u:p@api.example.com:8443/x?y"),
            Some("api.example.com")
        );
        assert_eq!(url_host("https://[::1]:8080/mcp"), Some("::1"));
        assert_eq!(url_host("api.example.com/mcp"), None, "ohne Schema");
        assert_eq!(url_host("ftp://api.example.com/x"), None, "fremdes Schema");
        assert_eq!(url_host("https:///mcp"), None, "leerer Host");
    }

    #[test]
    fn fs_prefix_allows_subpaths_only() {
        let pol = Policy::new(vec![Capability::FsWrite {
            prefix: p("/proj/out"),
        }]);
        assert!(pol.allows(&Capability::FsWrite {
            prefix: p("/proj/out/sub/x")
        }));
        assert!(pol.allows(&Capability::FsWrite {
            prefix: p("/proj/out")
        }));
        assert!(!pol.allows(&Capability::FsWrite {
            prefix: p("/proj/other")
        }));
        // Ein FsWrite-Grant schließt das Lesen ein — wie in `allows_path` und wie es beide
        // Sandbox-Adapter ohnehin eintragen. Umgekehrt gilt das nicht.
        assert!(pol.allows(&Capability::FsRead {
            prefix: p("/proj/out/x")
        }));
        assert!(!pol.allows(&Capability::FsRead {
            prefix: p("/proj/other")
        }));
    }

    #[test]
    fn net_glob_matches_subdomains_not_apex() {
        let pol = Policy::new(vec![
            Capability::Net {
                host: "*.example.com".into(),
            },
            Capability::Net {
                host: "api.foo.com".into(),
            },
        ]);
        assert!(pol.allows(&Capability::Net {
            host: "a.example.com".into()
        }));
        assert!(pol.allows(&Capability::Net {
            host: "x.y.example.com".into()
        }));
        assert!(!pol.allows(&Capability::Net {
            host: "example.com".into()
        }));
        assert!(pol.allows(&Capability::Net {
            host: "api.foo.com".into()
        }));
        assert!(!pol.allows(&Capability::Net {
            host: "evil.com".into()
        }));
    }

    #[test]
    fn env_and_exec_exact() {
        let pol = Policy::new(vec![
            Capability::Env {
                name: "TOKEN".into(),
            },
            Capability::Exec {
                program: "git".into(),
            },
        ]);
        assert!(pol.allows(&Capability::Env {
            name: "TOKEN".into()
        }));
        assert!(!pol.allows(&Capability::Env {
            name: "OTHER".into()
        }));
        assert!(pol.allows(&Capability::Exec {
            program: "git".into()
        }));
        assert!(!pol.allows(&Capability::Exec {
            program: "rm".into()
        }));
    }

    #[test]
    fn empty_policy_denies_everything() {
        let pol = Policy::default();
        assert!(!pol.allows(&Capability::Net {
            host: "x.com".into()
        }));
        assert!(!pol.allows(&Capability::FsRead { prefix: p("/") }));
    }

    #[test]
    fn manifest_parses_capabilities() {
        let toml = r#"
            name = "string-tools"
            version = "0.1.0"
            kind = "wasm"
            entry = "string_tools.wasm"

            [capabilities]
            fs_read = ["/abs/read"]
            net = ["api.example.com", "*.cdn.example.com"]
            env = ["LEVEL"]
        "#;
        let m = Manifest::parse(toml).unwrap();
        assert_eq!(m.name, "string-tools");
        assert_eq!(m.kind.as_deref(), Some("wasm"));
        let pol = m.policy();
        assert!(pol.allows(&Capability::FsRead {
            prefix: p("/abs/read/x")
        }));
        assert!(pol.allows(&Capability::Net {
            host: "sub.cdn.example.com".into()
        }));
        assert!(pol.allows(&Capability::Env {
            name: "LEVEL".into()
        }));
        assert!(!pol.allows(&Capability::Net {
            host: "evil.com".into()
        }));
    }

    #[test]
    fn manifest_abi_defaults_to_one_and_is_read() {
        let m = Manifest::parse("name = \"x\"").unwrap();
        assert_eq!(m.abi, 1, "ohne Angabe gilt Version 1");
        let m = Manifest::parse("name = \"x\"\nabi = 2").unwrap();
        assert_eq!(m.abi, 2);
    }

    #[test]
    fn manifest_collects_unknown_fields_without_failing() {
        // Ein Tippfehler soll das Manifest nicht ablehnen, aber auch nicht stumm verschwinden.
        let m = Manifest::parse(
            "name = \"x\"\nkind = \"wasm\"\nentry = \"x.wasm\"\n\
             gibtsnicht = 42\n\
             [capabilites]\nnet = [\"a\"]\n\
             [limits]\nmax_memory_pages = 8\n",
        )
        .unwrap();
        // Bekannte Felder landen NICHT in der Restkarte.
        assert_eq!(m.name, "x");
        assert_eq!(m.limits.max_memory_pages, 8);
        assert!(m.capabilities.net.is_empty(), "der Tippfehler zählt nicht");
        // Der Tippfehler und das unbekannte Feld schon.
        assert_eq!(m.unknown_keys(), vec!["capabilites", "gibtsnicht"]);
    }

    #[test]
    fn manifest_without_capabilities_is_pure() {
        let m = Manifest::parse("name=\"reverse\"\nkind=\"wasm\"\nentry=\"r.wasm\"").unwrap();
        assert!(m.policy().granted.is_empty());
    }

    #[test]
    fn manifest_without_limits_uses_conservative_defaults() {
        let m = Manifest::parse("name=\"reverse\"\nkind=\"wasm\"\nentry=\"r.wasm\"").unwrap();
        assert_eq!(m.limits, Limits::default());
        assert_eq!(m.limits.max_memory_pages, 256);
        assert_eq!(m.limits.max_wall_time_ms, 30_000);
        assert_eq!(m.limits.fuel_slice, 1_000_000);
        assert_eq!(m.limits.max_memory_bytes(), 16 * 1024 * 1024);
        assert_eq!(m.limits.max_http_requests, 16);
        assert_eq!(m.limits.max_http_response_bytes, 4 * 1024 * 1024);
        assert_eq!(m.limits.http_timeout_ms, 10_000);
    }

    #[test]
    fn manifest_parses_http_limits_and_keeps_the_rest_at_default() {
        let toml = r#"
            name = "konnektor"
            [limits]
            max_http_requests = 3
            max_http_response_bytes = 1024
            http_timeout_ms = 500
        "#;
        let m = Manifest::parse(toml).unwrap();
        assert_eq!(m.limits.max_http_requests, 3);
        assert_eq!(m.limits.max_http_response_bytes, 1024);
        assert_eq!(m.limits.http_timeout_ms, 500);
        assert_eq!(m.limits.max_memory_pages, 256);
    }

    #[test]
    fn manifest_parses_limits_section() {
        let toml = r#"
            name = "cruncher"
            kind = "wasm"
            entry = "c.wasm"

            [limits]
            max_memory_pages = 512
            max_wall_time_ms = 0
            fuel_slice = 50000
        "#;
        let m = Manifest::parse(toml).unwrap();
        assert_eq!(m.limits.max_memory_pages, 512);
        // 0 = unbegrenzt lange laufen dürfen (explizit erlaubt, aber unterbrechbar).
        assert_eq!(m.limits.max_wall_time_ms, 0);
        assert_eq!(m.limits.fuel_slice, 50_000);
    }

    #[test]
    fn wildcard_host_matches_everything() {
        let pol = Policy::new(vec![Capability::Net { host: "*".into() }]);
        assert!(pol.allows(&Capability::Net {
            host: "api.example.com".into()
        }));
        assert!(pol.allows(&Capability::Net {
            host: "localhost".into()
        }));
        assert!(pol.net_allowed());
        assert!(!Policy::default().net_allowed());
    }

    #[test]
    fn intersect_keeps_narrower_prefix_and_drops_uncovered() {
        let grant = Policy::new(vec![
            Capability::FsRead { prefix: p("/data") },
            Capability::Net {
                host: "*.example.com".into(),
            },
        ]);
        let request = Policy::new(vec![
            Capability::FsRead {
                prefix: p("/data/sub"),
            },
            Capability::Net {
                host: "api.example.com".into(),
            },
            Capability::Net {
                host: "evil.com".into(),
            },
            Capability::Env { name: "X".into() },
        ]);
        let eff = grant.intersect(&request);
        assert_eq!(
            eff.granted,
            vec![
                Capability::FsRead {
                    prefix: p("/data/sub")
                },
                Capability::Net {
                    host: "api.example.com".into()
                },
            ]
        );
        // Umgekehrt: Anfrage weiter als Gewährung → Gewährung bleibt.
        let eff2 = request.intersect(&grant);
        assert!(eff2.allows(&Capability::FsRead {
            prefix: p("/data/sub/x")
        }));
        assert!(!eff2.allows(&Capability::FsRead {
            prefix: p("/data/other")
        }));
    }

    #[test]
    fn write_grant_covers_a_read_request() {
        // Manifest fordert Lesen, gewährt ist Schreiben: Lesen bleibt übrig.
        let grant = Policy::new(vec![Capability::FsWrite { prefix: p("/data") }]);
        let request = Policy::new(vec![Capability::FsRead {
            prefix: p("/data/sub"),
        }]);
        assert_eq!(
            grant.intersect(&request).granted,
            vec![Capability::FsRead {
                prefix: p("/data/sub")
            }]
        );
        // Manifest fordert Schreiben, gewährt ist nur Lesen: Es bleibt beim Lesen, das Plugin
        // scheitert später beim Schreiben. Fail-closed in beide Richtungen.
        let read_only = Policy::new(vec![Capability::FsRead { prefix: p("/data") }]);
        let wants_write = Policy::new(vec![Capability::FsWrite {
            prefix: p("/data/sub"),
        }]);
        let eff = read_only.intersect(&wants_write);
        assert!(!eff.allows(&Capability::FsWrite {
            prefix: p("/data/sub")
        }));
    }

    #[test]
    fn read_grant_never_covers_a_write_request() {
        let read_only = Policy::new(vec![Capability::FsRead { prefix: p("/data") }]);
        assert!(!read_only.allows(&Capability::FsWrite {
            prefix: p("/data/sub")
        }));
        assert!(!read_only.allows_path(&p("/data/sub/x"), true));
    }

    #[test]
    fn union_dedups() {
        let a = Policy::new(vec![Capability::Env { name: "A".into() }]);
        let b = Policy::new(vec![
            Capability::Env { name: "A".into() },
            Capability::Env { name: "B".into() },
        ]);
        assert_eq!(a.union(&b).granted.len(), 2);
    }

    #[test]
    fn without_denied_drops_grant_under_deny_and_reports_overlap() {
        let pol = Policy::new(vec![
            Capability::FsRead {
                prefix: p("/home/u"),
            },
            Capability::FsRead {
                prefix: p("/home/u/.ssh/keys"),
            },
            Capability::FsWrite { prefix: p("/proj") },
            Capability::Net { host: "*".into() },
        ]);
        let rules = vec![
            DenyRule::read("/home/u/.ssh"),
            DenyRule::write("/proj/.sepp"),
        ];
        let (eff, overlaps) = pol.without_denied(&rules);
        // Grant unter Deny fällt weg …
        assert!(!eff.granted.contains(&Capability::FsRead {
            prefix: p("/home/u/.ssh/keys")
        }));
        // … Nicht-FS-Rechte bleiben …
        assert!(eff.net_allowed());
        // … Deny unter Grant wird gemeldet (read-Deny unter read-Grant, write-Deny unter write-Grant).
        assert_eq!(
            overlaps,
            vec![
                DenyOverlap {
                    grant: p("/home/u"),
                    deny: p("/home/u/.ssh")
                },
                DenyOverlap {
                    grant: p("/proj"),
                    deny: p("/proj/.sepp")
                },
            ]
        );
        // Die In-Process-Prüfung setzt das Deny trotzdem durch.
        assert!(!eff.allows_path(Path::new("/home/u/.ssh/id"), false));
        assert!(eff.allows_path(Path::new("/home/u/notes"), false));
        assert!(!eff.allows_path(Path::new("/proj/.sepp/policy.toml"), true));
        assert!(eff.allows_path(Path::new("/proj/.sepp/policy.toml"), false)); // write-Deny sperrt nur Schreiben
        assert!(eff.allows_path(Path::new("/proj/src/main.rs"), true));
    }

    #[test]
    fn write_grant_implies_read_for_allows_path() {
        let pol = Policy::new(vec![Capability::FsWrite { prefix: p("/w") }]);
        assert!(pol.allows_path(Path::new("/w/x"), false));
        assert!(pol.allows_path(Path::new("/w/x"), true));
        assert!(!pol.allows_path(Path::new("/r/x"), false));
    }

    #[test]
    fn exec_programs_none_when_unrestricted() {
        assert_eq!(Policy::default().exec_programs(), None);
        let pol = Policy::new(vec![Capability::Exec {
            program: "git".into(),
        }]);
        assert_eq!(pol.exec_programs(), Some(vec!["git".to_string()]));
    }

    fn ctx() -> ResolveCtx {
        ResolveCtx {
            home: Some(p("/home/u")),
            cwd: p("/proj"),
            tmpdir: p("/tmp/t"),
        }
    }

    #[test]
    fn resolve_path_with_expands_tilde_tmpdir_and_relative() {
        // Nicht existierende Pfade: canonicalize scheitert, die Expansion bleibt lexikalisch.
        assert_eq!(resolve_path_with("~/.ssh", &ctx()), p("/home/u/.ssh"));
        assert_eq!(resolve_path_with("~", &ctx()), p("/home/u"));
        assert_eq!(resolve_path_with("$TMPDIR", &ctx()), p("/tmp/t"));
        assert_eq!(resolve_path_with("$TMPDIR/x", &ctx()), p("/tmp/t/x"));
        assert_eq!(resolve_path_with("./out", &ctx()), p("/proj/out"));
        assert_eq!(resolve_path_with("/abs", &ctx()), p("/abs"));
        let no_home = ResolveCtx {
            home: None,
            ..ctx()
        };
        assert_eq!(resolve_path_with("~/x", &no_home), p("/proj/~/x"));
    }

    #[test]
    fn canonicalize_lenient_resolves_existing_ancestor_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = tmp.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        std::fs::create_dir_all(&link).unwrap();
        let real_c = real.canonicalize().unwrap();
        // Neue Datei unter dem Symlink → landet kanonisch unter `real`.
        let got = canonicalize_lenient(&link.join("sub").join("new.txt"));
        assert_eq!(got, real_c.join("sub").join("new.txt"));
        // `..` wird lexikalisch aufgelöst.
        let got = canonicalize_lenient(&real.join("a").join("..").join("b"));
        assert_eq!(got, real_c.join("b"));
        // Existierender Pfad → echtes canonicalize.
        assert_eq!(canonicalize_lenient(&link), real_c);
    }

    #[test]
    fn manifest_rejects_implausible_limits() {
        let zero_fuel = "name=\"x\"\n[limits]\nfuel_slice = 0";
        assert!(
            Manifest::parse(zero_fuel).is_err(),
            "fuel_slice=0 muss scheitern"
        );

        let huge_mem = "name=\"x\"\n[limits]\nmax_memory_pages = 100000";
        assert!(
            Manifest::parse(huge_mem).is_err(),
            "max_memory_pages>65536 muss scheitern"
        );

        let zero_mem = "name=\"x\"\n[limits]\nmax_memory_pages = 0";
        assert!(
            Manifest::parse(zero_mem).is_err(),
            "max_memory_pages=0 muss scheitern"
        );

        for bad in [
            "max_http_requests = 0",
            "max_http_response_bytes = 0",
            "max_http_response_bytes = 17825792", // 17 MiB > MAX_HTTP_RESPONSE_BYTES
            "http_timeout_ms = 0",
        ] {
            let toml = format!("name=\"x\"\n[limits]\n{bad}");
            let e = Manifest::parse(&toml).unwrap_err().to_string();
            assert!(e.contains("[limits]"), "{bad}: {e}");
        }
    }
}
