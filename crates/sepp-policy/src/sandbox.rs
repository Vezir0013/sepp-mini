//! Plattform-Sandbox für Subprozesse (MCP-Server u. a.). Linux: Landlock (LSM), macOS: Seatbelt
//! (`sandbox_init`); sonst ein portabler Fallback ohne Durchsetzung (mit deutlicher Warnung).

use sepp_core::{Result, SeppError};

use crate::{Capability, Policy};

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

/// Linux-Sandbox via Landlock (Dateisystem-Zugriff auf die Policy-Pfade begrenzt).
#[cfg(target_os = "linux")]
pub struct LandlockSandbox;

#[cfg(target_os = "linux")]
impl Sandbox for LandlockSandbox {
    fn prepare(&self, cmd: &mut tokio::process::Command, policy: &Policy) -> Result<()> {
        // Env-Capability durchsetzen (geerbte Secrets entfernen).
        scrub_env(cmd, policy);

        let mut read = policy.fs_read_prefixes();
        // Schreibpfade brauchen auch Lese-/Traversierungsrechte.
        read.extend(policy.fs_write_prefixes());
        let write = policy.fs_write_prefixes();

        // pre_exec läuft im Kind nach fork(), vor exec() — die Restriktion überlebt exec.
        unsafe {
            cmd.pre_exec(move || apply_landlock(&read, &write).map_err(std::io::Error::other));
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn apply_landlock(
    read: &[std::path::PathBuf],
    write: &[std::path::PathBuf],
) -> std::result::Result<(), String> {
    use landlock::{
        Access, AccessFs, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
        RulesetAttr, RulesetCreated, RulesetCreatedAttr, RulesetStatus, ABI,
    };
    use std::path::Path;

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

    let abi = ABI::V1;
    let read_acc = AccessFs::from_read(abi);
    let all_acc = AccessFs::from_all(abi);

    let mut created = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| e.to_string())?
        .create()
        .map_err(|e| e.to_string())?;

    // Systempfade: lesen+ausführen (damit Programm & Libs laufen).
    for p in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/proc"] {
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

    let status = created.restrict_self().map_err(|e| e.to_string())?;
    // Fail-closed: wenn der Kernel Landlock nicht durchsetzt (BestEffort → NotEnforced), den
    // Subprozess NICHT ungesandboxt starten. spawn() schlägt dann fehl (z. B. MCP-Server wird
    // übersprungen), statt das Sandbox-Versprechen still zu brechen.
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

/// Baut ein Seatbelt-Profil (SBPL) mit **Default-deny**: erlaubt nur die zum Start nötigen
/// System-Lesepfade sowie die per [`Policy`] gewährten Lese-/Schreibpfade. Reine, testbare
/// Funktion (plattformunabhängig, damit der Generator auch ohne macOS geprüft werden kann).
#[cfg(any(target_os = "macos", test))]
fn build_seatbelt_profile(read: &[std::path::PathBuf], write: &[std::path::PathBuf]) -> String {
    // macOS-Systempfade, die ein Prozess zum Starten braucht (dyld-Cache, Frameworks, Config).
    // Nur **Lesen** — analog zu Landlocks Systempfad-Set, an macOS angepasst.
    const SYSTEM_READ: &[&str] = &[
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

    let mut p = String::from("(version 1)\n(deny default)\n");
    // Prozess-Start (exec des Ziels + Kinder) und minimaler Runtime-Bedarf, damit dyld und
    // Frameworks laden. FS bleibt Default-deny — nur die folgenden Pfade sind erlaubt.
    p.push_str("(allow process-exec*)\n");
    p.push_str("(allow process-fork)\n");
    p.push_str("(allow sysctl-read)\n");
    p.push_str("(allow mach-lookup)\n");
    // Metadaten (stat/lookup) baumweit — gibt keine Datei-Inhalte frei, erlaubt Traversierung.
    p.push_str("(allow file-read-metadata)\n");
    // Geräte (stdin/stdout/null): lesen + schreiben (wie Landlock für /dev).
    p.push_str("(allow file-read* file-write* (subpath \"/dev\"))\n");

    for sys in SYSTEM_READ {
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
    p
}

/// macOS-Sandbox via Seatbelt (`sandbox_init`): Dateisystem-Zugriff auf die Policy-Pfade
/// begrenzt. Parität zu `LandlockSandbox` (Scope: Dateisystem + Env).
#[cfg(target_os = "macos")]
pub struct SeatbeltSandbox;

#[cfg(target_os = "macos")]
impl Sandbox for SeatbeltSandbox {
    fn prepare(&self, cmd: &mut tokio::process::Command, policy: &Policy) -> Result<()> {
        // Env-Capability durchsetzen (geerbte Secrets entfernen) — wie bei allen Adaptern.
        scrub_env(cmd, policy);

        let read = policy.fs_read_prefixes();
        let write = policy.fs_write_prefixes();
        // Das SBPL-Profil VOR dem fork bauen: im Kind nach fork() darf nur minimal (nicht
        // async-signal-safe) gearbeitet werden — siehe apply_seatbelt.
        let profile = build_seatbelt_profile(&read, &write);
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
    use std::path::PathBuf;

    #[test]
    fn profile_is_deny_default_with_policy_paths() {
        let read = vec![PathBuf::from("/tmp/ro")];
        let write = vec![PathBuf::from("/tmp/rw")];
        let p = build_seatbelt_profile(&read, &write);

        // Default-deny als Fundament.
        assert!(p.starts_with("(version 1)\n(deny default)\n"));
        // Gewährte Policy-Pfade tauchen exakt auf.
        assert!(p.contains("(allow file-read* (subpath \"/tmp/ro\"))"));
        assert!(p.contains("(allow file-read* file-write* (subpath \"/tmp/rw\"))"));
        // System-Lesepfade vorhanden, aber nur lesend.
        assert!(p.contains("(allow file-read* (subpath \"/usr\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/System\"))"));
        // Kein pauschaler Schreibzugriff außerhalb der gewährten Pfade.
        assert!(!p.contains("(allow file-write* (subpath \"/\"))"));
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
}
