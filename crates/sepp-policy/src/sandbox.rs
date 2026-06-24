//! Plattform-Sandbox für Subprozesse (MCP-Server u. a.). Linux: Landlock (LSM); sonst ein
//! portabler Fallback ohne Durchsetzung (mit deutlicher Warnung).

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
    #[cfg(not(target_os = "linux"))]
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
