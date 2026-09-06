//! Plattform-Sandbox für Subprozesse (bash-Tool, MCP-Server). Linux: Landlock (LSM), macOS:
//! Seatbelt (`sandbox_init`); sonst ein portabler Fallback ohne Durchsetzung (mit deutlicher Warnung).
//!
//! Was die Adapter aus einer [`Policy`] machen:
//! - `FsRead`/`FsWrite`-Präfixe → Dateisystem-Regeln (Systempfade immer lesbar).
//! - kein `Net`-Recht → TCP-Verbindungen verboten (Landlock ab ABI v4 / Kernel 6.7; Seatbelt ohne
//!   `network*`). Irgendein `Net`-Recht → TCP gesamt erlaubt; nach Host filtern kann keiner der
//!   Adapter (Egress-Proxy folgt).
//! - `Exec`-Einträge → Execute-Recht nur auf die genannten Programme (plus das gestartete
//!   Programm selbst); ohne `Exec`-Einträge bleibt Ausführen unbeschränkt.
//! - `Env`-Rechte → Allowlist beim Environment-Scrubbing (Default-deny).
//! - `denied` → Seatbelt trägt Deny-Zeilen ein; Landlock kann Verbote unterhalb einer Gewährung
//!   nicht ausdrücken (siehe [`Policy::without_denied`]).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use sepp_core::{Result, SeppError};

use crate::{Capability, Policy};
#[cfg(any(target_os = "macos", test))]
use crate::{DenyKind, DenyRule};

/// Minimale Env-Vars, ohne die kaum ein Programm startet (kein Secret-Charakter).
const ENV_ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TMPDIR",
];

/// Setzt das Environment des Kindes auf **Default-deny**: leeren, dann nur die per Policy
/// gewährten `Env`-Vars (+ eine minimale Lauf-Allowlist) durchreichen. So sehen Subprozesse
/// **keine** geerbten Secrets wie `ANTHROPIC_API_KEY` (Durchsetzung der `Env`-Capability).
fn scrub_env(cmd: &mut tokio::process::Command, policy: &Policy) {
    cmd.env_clear();
    for (k, v) in env_allowlist_pairs(policy, |k| std::env::var_os(k)) {
        cmd.env(k, v);
    }
}

/// Reine Logik (testbar): welche Env-Vars dürfen durch? Allowlist + per Policy gewährte `Env`.
fn env_allowlist_pairs(
    policy: &Policy,
    get: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<(String, std::ffi::OsString)> {
    let mut out = Vec::new();
    for key in ENV_ALLOWLIST {
        if let Some(v) = get(key) {
            out.push((key.to_string(), v));
        }
    }
    for cap in &policy.granted {
        if let Capability::Env { name } = cap {
            if let Some(v) = get(name) {
                out.push((name.clone(), v));
            }
        }
    }
    out
}

#[cfg(test)]
mod env_tests {
    use super::*;

    #[test]
    fn env_is_default_deny_only_allowlist_and_grants() {
        let get = |k: &str| -> Option<std::ffi::OsString> {
            match k {
                "PATH" => Some("/usr/bin".into()),
                "ANTHROPIC_API_KEY" => Some("sk-secret".into()),
                "MYVAR" => Some("v".into()),
                _ => None,
            }
        };
        let has = |ps: &[(String, std::ffi::OsString)], k: &str| ps.iter().any(|(n, _)| n == k);

        // Leere Policy: nur Allowlist (PATH), KEIN Secret, KEIN sonstiges geerbtes Var.
        let pairs = env_allowlist_pairs(&Policy::default(), get);
        assert!(has(&pairs, "PATH"));
        assert!(!has(&pairs, "ANTHROPIC_API_KEY"));
        assert!(!has(&pairs, "MYVAR"));

        // Gewährtes Env{MYVAR} kommt durch; das Secret bleibt blockiert.
        let pol = Policy::new(vec![Capability::Env {
            name: "MYVAR".into(),
        }]);
        let pairs = env_allowlist_pairs(&pol, get);
        assert!(has(&pairs, "MYVAR"));
        assert!(!has(&pairs, "ANTHROPIC_API_KEY"));
    }
}

/// Sperrt einen Subprozess gemäß [`Policy`] ein.
pub trait Sandbox: Send + Sync {
    /// Wendet die Restriktionen auf `cmd` an, **ohne** zu spawnen — für Aufrufer, die selbst
    /// spawnen (z. B. rmcps `TokioChildProcess`).
    fn prepare(&self, cmd: &mut tokio::process::Command, policy: &Policy) -> Result<()>;

    /// Spawnt `cmd` eingesperrt.
    fn spawn(
        &self,
        cmd: &mut tokio::process::Command,
        policy: &Policy,
    ) -> Result<tokio::process::Child> {
        self.prepare(cmd, policy)?;
        cmd.spawn()
            .map_err(|e| SeppError::Provider(format!("spawn (sandboxed): {e}")))
    }
}

/// Fallback **ohne** Durchsetzung — nur für Plattformen ohne Adapter.
pub struct NullSandbox;

impl Sandbox for NullSandbox {
    fn prepare(&self, cmd: &mut tokio::process::Command, policy: &Policy) -> Result<()> {
        // Env-Durchsetzung ist OS-unabhängig und greift auch ohne FS-Sandbox.
        scrub_env(cmd, policy);
        Ok(())
    }
}

/// Wählt den besten verfügbaren Sandbox-Adapter für die Plattform.
pub fn default_sandbox() -> Box<dyn Sandbox> {
    #[cfg(target_os = "linux")]
    {
        Box::new(LandlockSandbox)
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(SeatbeltSandbox)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        tracing::warn!(
            "kein OS-Sandbox-Adapter für diese Plattform — Erweiterungen laufen UNGESANDBOXT"
        );
        Box::new(NullSandbox)
    }
}

/// Was der laufende Kernel bzw. das OS für Kindprozesse tatsächlich durchsetzen kann.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCapabilities {
    /// Dateisystem-Grenzen (Landlock ≥ ABI v1 / Seatbelt).
    pub fs_enforceable: bool,
    /// TCP-Verbot (Landlock ≥ ABI v4, Kernel 6.7 / Seatbelt).
    pub net_enforceable: bool,
    /// Menschenlesbare Begründung für `sepp policy` und Start-Hinweise.
    pub detail: String,
}

/// Härtet den eigenen Prozess: kein Core-Dump, und `/proc/<pid>/environ`, `maps`, `mem`
/// gehören root — ein Kindprozess derselben UID kann die Umgebung von sepp (die API-Keys) nicht
/// mehr über procfs lesen. Ergänzt Landlocks Ptrace-Schranke für die Fälle ohne Sandbox
/// (`--mode yolo`, Plattformen ohne Adapter). Wirkt nur auf sepp selbst: `execve` setzt das Flag
/// für jedes Kind zurück, `ps`, Debugger und `/proc/self` der Kinder laufen wie zuvor. Auf
/// anderen Plattformen ein No-op. Liefert `false`, wenn der Kernel den Aufruf verweigert — kein
/// Grund abzubrechen, nur zu wissen.
pub fn harden_process() -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `prctl(PR_SET_DUMPABLE, 0)` nimmt nur Integer-Argumente und berührt keinen
        // Speicher; die übrigen Argumente sind für diese Option unbenutzt und müssen 0 sein.
        unsafe {
            libc::prctl(
                libc::PR_SET_DUMPABLE,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            ) == 0
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Fragt ab, was die Plattform durchsetzen kann — ohne den eigenen Prozess zu beschränken.
///
/// Linux: Wegwerf-Rulesets mit `CompatLevel::HardRequirement`; `create()` legt nur einen
/// Deskriptor an, beschränkt aber nichts (erst `restrict_self()` täte das). Die Crate rät davon
/// ab, Rulesets dynamisch nach der Kernel-ABI zu bauen — hier dient die Abfrage nur der Anzeige
/// und der fail-closed-Entscheidung; gebaut wird weiterhin fest mit `ABI::V7` + BestEffort.
pub fn kernel_capabilities() -> SandboxCapabilities {
    #[cfg(target_os = "linux")]
    {
        use landlock::{
            Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr, ABI,
        };
        let fs = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V1))
            .and_then(|r| r.create())
            .is_ok();
        let net = fs
            && Ruleset::default()
                .set_compatibility(CompatLevel::HardRequirement)
                .handle_access(AccessNet::ConnectTcp)
                .and_then(|r| r.create())
                .is_ok();
        let detail = match (fs, net) {
            (true, true) => "Landlock: Dateisystem und TCP-Verbot durchsetzbar".to_string(),
            (true, false) => {
                "Landlock: Dateisystem durchsetzbar, TCP-Verbot nicht (Kernel < 6.7 oder ABI < 4)"
                    .to_string()
            }
            _ => "Landlock nicht verfügbar (Kernel ohne Landlock oder nicht aktiviert)".to_string(),
        };
        SandboxCapabilities {
            fs_enforceable: fs,
            net_enforceable: net,
            detail,
        }
    }
    #[cfg(target_os = "macos")]
    {
        SandboxCapabilities {
            fs_enforceable: true,
            net_enforceable: true,
            detail: "Seatbelt (sandbox_init): Dateisystem und Netz durchsetzbar".to_string(),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        SandboxCapabilities {
            fs_enforceable: false,
            net_enforceable: false,
            detail: "kein OS-Sandbox-Adapter für diese Plattform (NullSandbox)".to_string(),
        }
    }
}

/// Startprobe: spawnt `sh -c true` durch den Adapter. Schlägt fehl, wenn die Sandbox nicht
/// angewendet werden kann (z. B. Landlock `NotEnforced`, Seatbelt-Fehler) — dann darf der Agent
/// nicht ungesandboxt starten (fail-closed; Ausweg ist explizit `--mode yolo`).
pub async fn probe_sandbox(sb: &dyn Sandbox) -> Result<()> {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = sb
        .spawn(&mut cmd, &Policy::default())
        .map_err(|e| SeppError::Config(format!("Sandbox-Probe: {e}")))?;
    let status = child
        .wait()
        .await
        .map_err(|e| SeppError::Config(format!("Sandbox-Probe (wait): {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(SeppError::Config(format!(
            "Sandbox-Probe: `sh -c true` endete mit {status} (fehlt `sh`, oder blockiert die Sandbox den Start?)"
        )))
    }
}

/// Löst einen Programmnamen wie eine Shell auf: enthält er einen `/`, gilt er als Pfad; sonst
/// wird `PATH` (übergeben, nicht gelesen — pur und testbar) nach der ersten ausführbaren Datei
/// durchsucht.
pub fn resolve_program(name: &str, path_env: Option<&OsStr>) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() || p.components().count() > 1 {
        return is_executable_file(p).then(|| p.to_path_buf());
    }
    for dir in std::env::split_paths(path_env?) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let cand = dir.join(name);
        if is_executable_file(&cand) {
            return Some(cand);
        }
    }
    None
}

fn is_executable_file(p: &Path) -> bool {
    let Ok(md) = std::fs::metadata(p) else {
        return false;
    };
    if !md.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn push_unique(out: &mut Vec<PathBuf>, p: PathBuf) {
    if let Ok(c) = p.canonicalize() {
        if !out.contains(&c) {
            out.push(c);
        }
    }
    if !out.contains(&p) {
        out.push(p);
    }
}

/// Dynamische Loader (`ld-linux-*.so`, `ld-musl-*.so`, `ld.so`): der Kernel öffnet sie beim
/// `execve` eines dynamisch gelinkten Programms mit Execute-Recht. Ohne diese Einträge könnte
/// mit einer Exec-Allowlist kein einziges Programm starten.
fn dynamic_loaders() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in ["/lib", "/lib64", "/usr/lib", "/usr/lib64"] {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("ld-") || name.starts_with("ld.") {
                push_unique(&mut out, entry.path());
            }
        }
    }
    out
}

/// Exec-Allowlist als aufgelöste Dateipfade: `None` = unbeschränkt (keine `Exec`-Einträge).
/// Das gestartete Programm selbst (`command[0]`) ist immer enthalten, Symlink-Ziele ebenfalls
/// (`/bin/sh` → `/usr/bin/dash`), dazu die dynamischen Loader. Läuft im Elternprozess, VOR
/// `pre_exec`.
fn exec_targets(policy: &Policy, cmd: &tokio::process::Command) -> Option<Vec<PathBuf>> {
    let progs = policy.exec_programs()?;
    let path_env = std::env::var_os("PATH");
    let program = cmd.as_std().get_program().to_string_lossy().into_owned();
    let mut out: Vec<PathBuf> = Vec::new();
    for name in progs.iter().chain(std::iter::once(&program)) {
        if let Some(p) = resolve_program(name, path_env.as_deref()) {
            push_unique(&mut out, p);
        }
    }
    for l in dynamic_loaders() {
        push_unique(&mut out, l);
    }
    Some(out)
}

/// Linux-Sandbox via Landlock: Dateisystem-Zugriff auf die Policy-Pfade begrenzt, TCP verboten
/// ohne `Net`-Recht, Ausführen auf die `Exec`-Programme begrenzt (falls deklariert).
#[cfg(target_os = "linux")]
pub struct LandlockSandbox;

#[cfg(target_os = "linux")]
impl Sandbox for LandlockSandbox {
    fn prepare(&self, cmd: &mut tokio::process::Command, policy: &Policy) -> Result<()> {
        // Env-Capability durchsetzen (geerbte Secrets entfernen).
        scrub_env(cmd, policy);

        // Alles, was allokiert oder Env liest, passiert HIER im Elternprozess; die Closure
        // bekommt nur fertige Daten (nach fork() ist im Kind nur Minimales erlaubt).
        let mut read = policy.fs_read_prefixes();
        // Schreibpfade brauchen auch Lese-/Traversierungsrechte.
        read.extend(policy.fs_write_prefixes());
        let write = policy.fs_write_prefixes();
        let exec = exec_targets(policy, cmd);
        let deny_net = !policy.net_allowed();

        // pre_exec läuft im Kind nach fork(), vor exec() — die Restriktion überlebt exec.
        unsafe {
            cmd.pre_exec(move || {
                apply_landlock(&read, &write, exec.as_deref(), deny_net)
                    .map_err(std::io::Error::other)
            });
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn apply_landlock(
    read: &[PathBuf],
    write: &[PathBuf],
    exec: Option<&[PathBuf]>,
    deny_net: bool,
) -> std::result::Result<(), String> {
    use landlock::{
        Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd,
        Ruleset, RulesetAttr, RulesetCreated, RulesetCreatedAttr, RulesetStatus, ABI,
    };

    fn add(
        created: RulesetCreated,
        path: &Path,
        acc: BitFlags<AccessFs>,
    ) -> std::result::Result<RulesetCreated, String> {
        match PathFd::new(path) {
            Ok(fd) => created
                .add_rule(PathBeneath::new(fd, acc))
                .map_err(|e| e.to_string()),
            // Nicht vorhandene Pfade überspringen (best effort).
            Err(_) => Ok(created),
        }
    }

    // Fest V7 + BestEffort: Rechte, die der Kernel nicht kennt, fallen still weg (Status wird
    // dann `PartiallyEnforced`, nicht `NotEnforced`). V2 Refer, V3 Truncate, V4 TCP, V5 IoctlDev.
    let abi = ABI::V7;
    let restrict_exec = exec.is_some();
    // Mit Exec-Allowlist bekommen Systempfade KEIN Execute-Recht mehr — nur die Programme.
    let read_acc: BitFlags<AccessFs> = if restrict_exec {
        AccessFs::ReadFile | AccessFs::ReadDir
    } else {
        AccessFs::from_read(abi)
    };
    let mut all_acc = AccessFs::from_all(abi);
    if restrict_exec {
        all_acc.remove(AccessFs::Execute);
    }

    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| e.to_string())?;
    if deny_net {
        // Netz-Rechte handhaben, aber KEINE NetPort-Regel eintragen ⇒ TCP verboten (ab v4).
        ruleset = ruleset
            .handle_access(AccessNet::BindTcp)
            .map_err(|e| e.to_string())?;
        ruleset = ruleset
            .handle_access(AccessNet::ConnectTcp)
            .map_err(|e| e.to_string())?;
    }
    let mut created = ruleset.create().map_err(|e| e.to_string())?;

    // Systempfade: lesen (+ ausführen, sofern keine Exec-Allowlist), damit Programm & Libs laufen.
    for p in crate::system_read_paths() {
        created = add(created, Path::new(p), read_acc)?;
    }
    // Geräte (stdin/stdout/null): lesen+schreiben.
    created = add(created, Path::new("/dev"), all_acc)?;

    for p in read {
        created = add(created, p, read_acc)?;
    }
    for p in write {
        created = add(created, p, all_acc)?;
    }
    if let Some(progs) = exec {
        for p in progs {
            created = add(created, p, AccessFs::Execute | AccessFs::ReadFile)?;
        }
    }

    let status = created.restrict_self().map_err(|e| e.to_string())?;
    // Fail-closed: wenn der Kernel Landlock nicht durchsetzt (BestEffort → NotEnforced), den
    // Subprozess NICHT ungesandboxt starten. spawn() schlägt dann fehl (z. B. MCP-Server wird
    // übersprungen, das bash-Tool meldet den Fehler), statt das Sandbox-Versprechen still zu brechen.
    if matches!(status.ruleset, RulesetStatus::NotEnforced) {
        return Err(
            "Landlock wird auf diesem Kernel nicht durchgesetzt — Start abgebrochen (fail-closed)"
                .into(),
        );
    }
    Ok(())
}

/// Quotet einen Pfad als SBPL-String-Literal (escaped `\` und `"`).
#[cfg(any(target_os = "macos", test))]
fn sbpl_quote(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 2);
    out.push('"');
    for c in path.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// macOS-Systempfade, die ein Prozess zum Starten braucht (dyld-Cache, Frameworks, Config).
/// Nur **Lesen** — analog zu Landlocks Systempfad-Set, an macOS angepasst. Bewusst hier
/// fest verdrahtet (nicht `system_read_paths()`), damit der Profil-Generator auf jeder
/// Plattform dasselbe macOS-Profil erzeugt und testbar bleibt.
#[cfg(any(target_os = "macos", test))]
const SEATBELT_SYSTEM_READ: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library",
    "/private/etc",
    "/private/var/db/dyld",
    "/opt",
    "/Applications",
];

/// Baut ein Seatbelt-Profil (SBPL) mit **Default-deny**: erlaubt nur die zum Start nötigen
/// System-Lesepfade sowie die per [`Policy`] gewährten Lese-/Schreibpfade; Netz nur mit `net`;
/// Ausführen nur der `exec`-Programme, falls eine Liste vorliegt; `deny`-Zeilen zuletzt (SBPL:
/// die letzte passende Regel gewinnt). Reine, testbare Funktion (plattformunabhängig).
///
/// Hinweis zu Exec-Listen auf macOS: Apple-Shims (`/usr/bin/git` startet über `xcrun` das echte
/// Binary unter `/Library/Developer/CommandLineTools`) machen `literal`-Regeln fragil; die
/// Symlink-Ziele werden mit aufgenommen, Shims aber nicht aufgelöst.
#[cfg(any(target_os = "macos", test))]
fn build_seatbelt_profile(
    read: &[PathBuf],
    write: &[PathBuf],
    deny: &[DenyRule],
    exec: Option<&[PathBuf]>,
    net: bool,
) -> String {
    let mut p = String::from("(version 1)\n(deny default)\n");
    // Prozess-Start (exec des Ziels + Kinder) und minimaler Runtime-Bedarf, damit dyld und
    // Frameworks laden. FS bleibt Default-deny — nur die folgenden Pfade sind erlaubt.
    match exec {
        Some(progs) => {
            for prog in progs {
                if let Some(s) = prog.to_str() {
                    p.push_str(&format!(
                        "(allow process-exec (literal {}))\n",
                        sbpl_quote(s)
                    ));
                }
            }
        }
        None => p.push_str("(allow process-exec*)\n"),
    }
    p.push_str("(allow process-fork)\n");
    p.push_str("(allow sysctl-read)\n");
    p.push_str("(allow mach-lookup)\n");
    if net {
        p.push_str("(allow network*)\n");
    }
    // Pfad-Traversierung: Metadaten (stat/lookup) baumweit — gibt KEINE Datei-Inhalte frei.
    p.push_str("(allow file-read-metadata (subpath \"/\"))\n");
    // Der Root-Knoten braucht read-data (dyld liest beim Start das „/“-Directory). `literal` = nur
    // der „/“-Knoten selbst, NICHT rekursiv → Kindpfade bleiben Default-deny (Read bleibt confined).
    p.push_str("(allow file-read* (literal \"/\"))\n");
    // Geräte (stdin/stdout/null): lesen + schreiben (wie Landlock für /dev); `file-ioctl` deckt den
    // dtracehelper-Zugriff beim libc-Start ab (sonst harmlose, aber laute Sandbox-Violation).
    p.push_str("(allow file-read* file-write* (subpath \"/dev\"))\n");
    p.push_str("(allow file-ioctl (subpath \"/dev\"))\n");

    for sys in SEATBELT_SYSTEM_READ {
        p.push_str(&format!(
            "(allow file-read* (subpath {}))\n",
            sbpl_quote(sys)
        ));
    }
    for r in read {
        if let Some(s) = r.to_str() {
            p.push_str(&format!("(allow file-read* (subpath {}))\n", sbpl_quote(s)));
        }
    }
    for w in write {
        if let Some(s) = w.to_str() {
            p.push_str(&format!(
                "(allow file-read* file-write* (subpath {}))\n",
                sbpl_quote(s)
            ));
        }
    }
    // Verbote zuletzt: sie schlagen die Allows darüber (auch die Systempfade).
    for d in deny {
        if let Some(s) = d.prefix.to_str() {
            match d.kind {
                DenyKind::Read => p.push_str(&format!(
                    "(deny file-read* file-write* (subpath {}))\n",
                    sbpl_quote(s)
                )),
                DenyKind::Write => {
                    p.push_str(&format!("(deny file-write* (subpath {}))\n", sbpl_quote(s)))
                }
            }
        }
    }
    p
}

/// Löst Policy-Pfade zu ihrem kanonischen (realpath-)Pfad auf. Seatbelt matcht kanonisch, und auf
/// macOS sind Verzeichnisse wie `/var` oder `/tmp` Symlinks (`/var` → `/private/var`) — ohne
/// Auflösung würde die erlaubte Regel den echten Zugriffspfad verfehlen. Best effort: existiert der
/// Pfad (noch) nicht, bleibt das Original erhalten.
#[cfg(target_os = "macos")]
fn canonicalize_all(paths: Vec<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
    paths
        .into_iter()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .collect()
}

/// macOS-Sandbox via Seatbelt (`sandbox_init`): Dateisystem-Zugriff auf die Policy-Pfade
/// begrenzt, Netz nur mit `Net`-Recht, Ausführen nur der `Exec`-Programme (falls deklariert),
/// Verbote als Deny-Zeilen. Parität zu `LandlockSandbox` (Scope: Dateisystem + Netz + Exec + Env).
#[cfg(target_os = "macos")]
pub struct SeatbeltSandbox;

#[cfg(target_os = "macos")]
impl Sandbox for SeatbeltSandbox {
    fn prepare(&self, cmd: &mut tokio::process::Command, policy: &Policy) -> Result<()> {
        // Env-Capability durchsetzen (geerbte Secrets entfernen) — wie bei allen Adaptern.
        scrub_env(cmd, policy);

        // Seatbelt matcht KANONISCHE Pfade; auf macOS sind /var, /tmp u. a. Symlinks
        // (/var → /private/var) — Policy-Pfade vor dem Profil-Eintrag auflösen, sonst greift die
        // erlaubte Regel nicht.
        let read = canonicalize_all(policy.fs_read_prefixes());
        let write = canonicalize_all(policy.fs_write_prefixes());
        let deny: Vec<DenyRule> = policy
            .denied
            .iter()
            .map(|d| DenyRule {
                prefix: std::fs::canonicalize(&d.prefix).unwrap_or_else(|_| d.prefix.clone()),
                kind: d.kind,
            })
            .collect();
        let exec = exec_targets(policy, cmd);
        // Das SBPL-Profil VOR dem fork bauen: im Kind nach fork() darf nur minimal (nicht
        // async-signal-safe) gearbeitet werden — siehe apply_seatbelt.
        let profile =
            build_seatbelt_profile(&read, &write, &deny, exec.as_deref(), policy.net_allowed());
        let profile = std::ffi::CString::new(profile)
            .map_err(|e| SeppError::Provider(format!("seatbelt: Profil enthält NUL: {e}")))?;

        // pre_exec läuft im Kind nach fork(), vor exec() — die Restriktion überlebt exec.
        unsafe {
            cmd.pre_exec(move || apply_seatbelt(profile.as_c_str()).map_err(std::io::Error::other));
        }
        Ok(())
    }
}

/// Wendet ein SBPL-Profil auf den **aktuellen** Prozess an (im Kind, vor exec).
///
/// Nutzt `sandbox_init` aus libSystem/`libsandbox` (seit macOS 10.8 als deprecated markiert, aber
/// weiter stabil und u. a. von Chromium verwendet). Fehler → `Err`, damit exec abbricht: der
/// Subprozess startet dann **nicht** ungesandboxt (**fail-closed**, spiegelt Landlocks
/// `NotEnforced`-Abbruch).
#[cfg(target_os = "macos")]
fn apply_seatbelt(profile: &std::ffi::CStr) -> std::result::Result<(), String> {
    use std::os::raw::{c_char, c_int};

    // `flags = 0` interpretiert `profile` als rohes SBPL — identisch zu `sandbox-exec -p`.
    #[link(name = "sandbox")]
    extern "C" {
        fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
        fn sandbox_free_error(errorbuf: *mut c_char);
    }

    let mut errbuf: *mut c_char = std::ptr::null_mut();
    // SAFETY: `profile` ist ein gültiger, NUL-terminierter C-String; `errbuf` zeigt auf einen
    // Null-Pointer, den `sandbox_init` bei Fehler mit einer Meldung befüllt.
    let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut errbuf) };
    if rc != 0 {
        let msg = if errbuf.is_null() {
            "unbekannter Fehler".to_string()
        } else {
            // SAFETY: bei Fehler zeigt `errbuf` auf einen NUL-terminierten String von sandbox_init.
            let s = unsafe { std::ffi::CStr::from_ptr(errbuf) }
                .to_string_lossy()
                .into_owned();
            // SAFETY: `errbuf` stammt aus sandbox_init und wird genau einmal freigegeben.
            unsafe { sandbox_free_error(errbuf) };
            s
        };
        return Err(format!(
            "Seatbelt (sandbox_init) fehlgeschlagen — Start abgebrochen (fail-closed): {msg}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod seatbelt_profile_tests {
    use super::*;

    #[test]
    fn profile_is_deny_default_with_policy_paths() {
        let read = vec![PathBuf::from("/tmp/ro")];
        let write = vec![PathBuf::from("/tmp/rw")];
        let p = build_seatbelt_profile(&read, &write, &[], None, false);

        // Default-deny als Fundament.
        assert!(p.starts_with("(version 1)\n(deny default)\n"));
        // Gewährte Policy-Pfade tauchen exakt auf.
        assert!(p.contains("(allow file-read* (subpath \"/tmp/ro\"))"));
        assert!(p.contains("(allow file-read* file-write* (subpath \"/tmp/rw\"))"));
        // System-Lesepfade vorhanden, aber nur lesend.
        assert!(p.contains("(allow file-read* (subpath \"/usr\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/System\"))"));
        // Start-/Traversierungs-kritisch: Root-Metadaten baumweit + read-data NUR auf dem
        // „/“-Knoten selbst (literal, nicht rekursiv).
        assert!(p.contains("(allow file-read-metadata (subpath \"/\"))"));
        assert!(p.contains("(allow file-read* (literal \"/\"))"));
        // Kein pauschaler Schreibzugriff außerhalb der gewährten Pfade.
        assert!(!p.contains("(allow file-write* (subpath \"/\"))"));
        // Read bleibt confined: KEIN rekursives read auf „/“.
        assert!(!p.contains("(allow file-read* (subpath \"/\"))"));
        // Ohne Exec-Liste: Ausführen unbeschränkt; ohne net: KEIN Netz.
        assert!(p.contains("(allow process-exec*)"));
        assert!(!p.contains("network"));
    }

    #[test]
    fn profile_has_network_only_with_net() {
        let with = build_seatbelt_profile(&[], &[], &[], None, true);
        assert!(with.contains("(allow network*)\n"));
        let without = build_seatbelt_profile(&[], &[], &[], None, false);
        assert!(!without.contains("network"));
    }

    #[test]
    fn profile_exec_literal_list_replaces_wildcard() {
        let progs = vec![PathBuf::from("/bin/sh"), PathBuf::from("/usr/bin/git")];
        let p = build_seatbelt_profile(&[], &[], &[], Some(&progs), false);
        assert!(!p.contains("(allow process-exec*)"));
        assert!(p.contains("(allow process-exec (literal \"/bin/sh\"))"));
        assert!(p.contains("(allow process-exec (literal \"/usr/bin/git\"))"));
    }

    #[test]
    fn profile_emits_deny_lines_after_allows() {
        let read = vec![PathBuf::from("/home/u")];
        let deny = vec![
            DenyRule::read("/home/u/.ssh"),
            DenyRule::write("/home/u/proj/.sepp"),
        ];
        let p = build_seatbelt_profile(&read, &[], &deny, None, false);
        let allow_pos = p.find("(allow file-read* (subpath \"/home/u\"))").unwrap();
        let deny_pos = p
            .find("(deny file-read* file-write* (subpath \"/home/u/.ssh\"))")
            .unwrap();
        assert!(deny_pos > allow_pos, "Deny muss nach dem Allow stehen");
        assert!(p.contains("(deny file-write* (subpath \"/home/u/proj/.sepp\"))"));
    }

    #[test]
    fn resolve_program_finds_first_executable_on_path() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        // In `a` nur eine nicht-ausführbare Datei, in `b` die ausführbare.
        std::fs::write(a.join("tool"), "x").unwrap();
        let exe = b.join("tool");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path_env = std::env::join_paths([a.clone(), b.clone()]).unwrap();
        #[cfg(unix)]
        assert_eq!(resolve_program("tool", Some(&path_env)), Some(exe.clone()));
        assert_eq!(resolve_program("missing", Some(&path_env)), None);
        assert_eq!(resolve_program("tool", None), None);
        // Mit `/` gilt der Name als Pfad.
        assert_eq!(
            resolve_program(exe.to_str().unwrap(), None),
            Some(exe.clone())
        );
        assert_eq!(resolve_program("/nonexistent/x", None), None);
    }

    #[test]
    fn kernel_capabilities_is_consistent() {
        let caps = kernel_capabilities();
        assert!(!caps.detail.is_empty());
        // Netz-Verbot setzt eine Dateisystem-Sandbox voraus.
        assert!(!caps.net_enforceable || caps.fs_enforceable);
    }

    #[test]
    fn sbpl_quote_escapes_quotes_and_backslashes() {
        assert_eq!(sbpl_quote("/a/b"), "\"/a/b\"");
        assert_eq!(sbpl_quote("/a\"b"), "\"/a\\\"b\"");
        assert_eq!(sbpl_quote("/a\\b"), "\"/a\\\\b\"");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod seatbelt_tests {
    use super::*;
    use std::process::Stdio;

    // Gated wie der Landlock-Test: braucht durchsetzbares Seatbelt (echter macOS-Host).
    // Lauf: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "braucht macOS-Seatbelt"]
    async fn seatbelt_blocks_write_outside_allowed_prefix() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ok = allowed.path().join("ok.txt");
        let escaped = outside.path().join("escaped.txt");

        let policy = Policy::new(vec![
            Capability::FsWrite {
                prefix: allowed.path().to_path_buf(),
            },
            Capability::FsRead {
                prefix: allowed.path().to_path_buf(),
            },
        ]);
        let sb = SeatbeltSandbox;

        let mut good = tokio::process::Command::new("sh");
        good.arg("-c")
            .arg(format!("echo hi > '{}'", ok.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = sb.spawn(&mut good, &policy).unwrap().wait().await;

        let mut bad = tokio::process::Command::new("sh");
        bad.arg("-c")
            .arg(format!("echo hi > '{}'", escaped.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = sb.spawn(&mut bad, &policy).unwrap().wait().await;

        // Positiv-Kontrolle: erlaubter Schreibzugriff klappt.
        assert!(
            ok.exists(),
            "erlaubter Schreibzugriff schlug fehl (Sandbox zu streng/inaktiv)"
        );
        // Negativ: Schreibzugriff außerhalb des erlaubten Pfads ist blockiert.
        assert!(
            !escaped.exists(),
            "Seatbelt verhinderte den Schreibzugriff außerhalb des erlaubten Pfads NICHT"
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    /// 0.5.1: sepp selbst ist nicht dumpbar — `/proc/<sepp>/environ` gehört danach root, ein
    /// Kind derselben UID kommt ohne Sandbox nicht mehr an die API-Keys.
    #[cfg(target_os = "linux")]
    #[test]
    fn harden_process_makes_the_process_non_dumpable() {
        assert!(super::harden_process());
        // SAFETY: reine Abfrage ohne Speicherzugriff.
        let dumpable = unsafe {
            libc::prctl(
                libc::PR_GET_DUMPABLE,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            )
        };
        assert_eq!(dumpable, 0);
    }

    use super::*;
    use std::process::Stdio;

    // Gated wie die Live-/Security-Tests: braucht durchsetzbares Landlock (echter Linux-Host;
    // in verschachtelten Sandboxes evtl. blockiert). Lauf: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "braucht durchsetzbares Landlock"]
    async fn landlock_blocks_write_outside_allowed_prefix() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ok = allowed.path().join("ok.txt");
        let escaped = outside.path().join("escaped.txt");

        let policy = Policy::new(vec![
            Capability::FsWrite {
                prefix: allowed.path().to_path_buf(),
            },
            Capability::FsRead {
                prefix: allowed.path().to_path_buf(),
            },
        ]);
        let sb = LandlockSandbox;

        let mut good = tokio::process::Command::new("sh");
        good.arg("-c")
            .arg(format!("echo hi > '{}'", ok.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = sb.spawn(&mut good, &policy).unwrap().wait().await;

        let mut bad = tokio::process::Command::new("sh");
        bad.arg("-c")
            .arg(format!("echo hi > '{}'", escaped.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = sb.spawn(&mut bad, &policy).unwrap().wait().await;

        // Positiv-Kontrolle: erlaubter Schreibzugriff klappt.
        assert!(
            ok.exists(),
            "erlaubter Schreibzugriff schlug fehl (Sandbox zu streng/inaktiv)"
        );
        // Negativ: Schreibzugriff außerhalb des erlaubten Pfads ist blockiert.
        assert!(
            !escaped.exists(),
            "Landlock verhinderte den Schreibzugriff außerhalb des erlaubten Pfads NICHT"
        );
    }

    /// `sh -c <cmd>` durch die Landlock-Sandbox, liefert den Exit-Status.
    async fn run_sandboxed(
        policy: &Policy,
        program: &str,
        script: &str,
    ) -> std::process::ExitStatus {
        let mut cmd = tokio::process::Command::new(program);
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        LandlockSandbox
            .spawn(&mut cmd, policy)
            .expect("spawn")
            .wait()
            .await
            .expect("wait")
    }

    #[tokio::test]
    #[ignore = "braucht durchsetzbares Landlock ≥ ABI v4 (Kernel 6.7) und bash"]
    async fn landlock_denies_tcp_connect_without_net_and_allows_with_net() {
        if !kernel_capabilities().net_enforceable {
            eprintln!("übersprungen: Kernel ohne Landlock-Netzregeln");
            return;
        }
        if resolve_program("bash", std::env::var_os("PATH").as_deref()).is_none() {
            eprintln!("übersprungen: bash fehlt");
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let script = format!("exec 3<>/dev/tcp/127.0.0.1/{port}");

        let denied = run_sandboxed(&Policy::default(), "bash", &script).await;
        assert!(
            !denied.success(),
            "TCP-Connect ohne Net-Recht muss scheitern"
        );

        let allowed = run_sandboxed(
            &Policy::new(vec![Capability::Net { host: "*".into() }]),
            "bash",
            &script,
        )
        .await;
        assert!(allowed.success(), "TCP-Connect mit Net-Recht muss klappen");
    }

    #[tokio::test]
    #[ignore = "braucht durchsetzbares Landlock"]
    async fn landlock_exec_allowlist_blocks_unlisted_program() {
        // Nur `sh` erlaubt: Builtins laufen, `ls` (fremdes Programm) nicht.
        let only_sh = Policy::new(vec![Capability::Exec {
            program: "sh".into(),
        }]);
        assert!(run_sandboxed(&only_sh, "sh", "true").await.success());
        assert!(!run_sandboxed(&only_sh, "sh", "ls /usr").await.success());

        // `sh` + `ls` erlaubt → läuft.
        let sh_and_ls = Policy::new(vec![
            Capability::Exec {
                program: "sh".into(),
            },
            Capability::Exec {
                program: "ls".into(),
            },
        ]);
        assert!(run_sandboxed(&sh_and_ls, "sh", "ls /usr").await.success());
    }
}
