//! Die Installation gegen abstrakte Wurzeln — testbar ohne Umgebung, ohne Binary.
//!
//! Ablauf in drei Schritten, die der Aufrufer (die CLI) mit Dialogen verbindet:
//! [`plan_install`] (Signatur, Manifest, Vorgänger, Vertrauen, Plugin-Manifeste) →
//! Variablen und Rechte auflösen, [`check_rights`], [`check_collisions`], [`consent_lines`] →
//! [`apply_install`]. Erst `apply` schreibt: Entpacken in ein Staging-Verzeichnis, Umbenennen,
//! Policy-Block, Nachweis. Verzeichnis vor Policy, weil ein Plugin ohne Block nichts darf (ein
//! sicherer Fehlzustand), ein Block ohne Verzeichnis nur verwirrt.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use sepp_core::{Result, SeppError};
use sepp_policy::fsutil::{ensure_private_dir, write_atomic};
use sepp_policy::policy_edit::{remove_package_section, write_package_section};
use sepp_policy::{Actor, Capability, Grants, Manifest, NetGrant, Policy, ResolveCtx};

use crate::container::{PkgArchive, Signed};
use crate::crypto::KeyFiles;
use crate::manifest::{Inventory, PkgManifest};
use crate::trust::{check_trust, now, TrustStatus};

/// Wo Pakete und Nachweise liegen. Content unter `config`, Nachweise und Schlüssel unter `state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    pub config: PathBuf,
    pub state: PathBuf,
}

impl Roots {
    /// `<config>/pkg` — je Paket ein Unterverzeichnis, das die Loader als Wurzel lesen.
    pub fn pkg_dir(&self) -> PathBuf {
        self.config.join("pkg")
    }
    pub fn package_dir(&self, name: &str) -> PathBuf {
        self.pkg_dir().join(name)
    }
    pub fn policy_path(&self) -> PathBuf {
        self.config.join("policy.toml")
    }
    pub fn user_plugins_dir(&self) -> PathBuf {
        self.config.join("plugins")
    }
    pub fn user_prompts_dir(&self) -> PathBuf {
        self.config.join("prompts")
    }
    pub fn user_skills_dir(&self) -> PathBuf {
        self.config.join("skills")
    }
    /// `<state>/pkg` — `installed.json` und das eigene Schlüsselpaar (0700).
    pub fn state_pkg_dir(&self) -> PathBuf {
        self.state.join("pkg")
    }
    pub fn installed_path(&self) -> PathBuf {
        self.state_pkg_dir().join("installed.json")
    }
    pub fn key_files(&self) -> KeyFiles {
        KeyFiles::in_dir(&self.state_pkg_dir())
    }
    /// `<state>/trusted-keys` — ein JSON je Herausgeber (0700/0600).
    pub fn trusted_keys_dir(&self) -> PathBuf {
        self.state.join("trusted-keys")
    }
    /// Die Verzeichnisse installierter Pakete, sortiert.
    pub fn package_dirs(&self) -> Vec<PathBuf> {
        package_dirs_in(&self.pkg_dir())
    }
}

/// Alle Paketverzeichnisse unter `pkg_root`, alphabetisch; Einträge mit `.`-Präfix
/// (`.staging-*`, `.old-*`) und Dateien werden übersprungen; ein fehlendes `pkg/` ist leer.
/// Dieselbe Funktion nutzt das CLI, um die Loader zu füttern — eine Regel, ein Ort.
pub fn package_dirs_in(pkg_root: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(pkg_root) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| {
            e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && !e.file_name().to_string_lossy().starts_with('.')
        })
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// Der Nachweis unter `<state>/pkg/installed.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    #[serde(default = "one")]
    pub format: u32,
    #[serde(default)]
    pub packages: BTreeMap<String, InstalledEntry>,
}

fn one() -> u32 {
    1
}

/// Ein installiertes Paket, wie `installed.json` es kennt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledEntry {
    pub version: String,
    /// Unix-Sekunden.
    pub installed_at: u64,
    pub publisher: String,
    pub publisher_fp: String,
    /// Die bei der Installation gesetzten Variablen — ein Upgrade übernimmt sie.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub files: usize,
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub hooks: Vec<String>,
}

impl Installed {
    pub fn load(roots: &Roots) -> Result<Self> {
        let path = roots.installed_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Installed {
                format: 1,
                packages: BTreeMap::new(),
            }),
            Err(e) => Err(SeppError::Config(format!("pkg: {}: {e}", path.display()))),
        }
    }

    pub fn save(&self, roots: &Roots) -> Result<()> {
        ensure_private_dir(&roots.state_pkg_dir())?;
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| SeppError::Config(format!("pkg: installed.json: {e}")))?;
        write_atomic(&roots.installed_path(), text.as_bytes(), Some(0o600))
    }
}

/// Alles, was vor der Zustimmung bekannt ist.
#[derive(Debug)]
pub struct InstallPlan {
    pub signed: Signed,
    pub inventory: Inventory,
    /// Der Nachweis der bereits installierten Version (Upgrade), falls vorhanden.
    pub previous: Option<InstalledEntry>,
    pub trust: TrustStatus,
    /// Die Plugin-Manifeste aus dem Archiv, je Stamm — die Selbstauskunft, gegen die
    /// `[rights]` geprüft wird.
    pub plugin_manifests: BTreeMap<String, Manifest>,
}

impl InstallPlan {
    pub fn manifest(&self) -> &PkgManifest {
        &self.signed.manifest
    }
    pub fn name(&self) -> &str {
        &self.signed.manifest.name
    }
}

/// Prüft Signatur und Manifest, liest die Plugin-Manifeste, vergleicht mit dem Vorgänger und
/// bestimmt das Vertrauen. Schreibt nichts.
pub fn plan_install(roots: &Roots, archive: &PkgArchive) -> Result<InstallPlan> {
    let signed = archive.read_signed_manifest()?;
    let manifest = &signed.manifest;
    let installed = Installed::load(roots)?;
    let previous = installed.packages.get(&manifest.name).cloned();
    if let Some(prev) = &previous {
        if prev.publisher != manifest.publisher.name {
            return Err(SeppError::Config(format!(
                "pkg: {} ist bereits von Herausgeber {} installiert, dieses Paket stammt von {} \
                 — ein Paketname gehört einem Herausgeber; erst `sepp pkg remove {}`",
                manifest.name, prev.publisher, manifest.publisher.name, manifest.name
            )));
        }
        let old = semver::Version::parse(&prev.version).ok();
        let new = semver::Version::parse(&manifest.version).ok();
        match (old, new) {
            (Some(o), Some(n)) if n <= o => {
                return Err(SeppError::Config(format!(
                    "pkg: {} {} ist bereits installiert; {} ist nicht neuer — zum Erneuern erst \
                     `sepp pkg remove {}`",
                    manifest.name, prev.version, manifest.version, manifest.name
                )))
            }
            _ => {}
        }
    }
    let trust = check_trust(roots, &manifest.publisher)?;
    let mut plugin_manifests = BTreeMap::new();
    for (stem, text) in archive.read_plugin_manifests(&signed)? {
        let m = Manifest::parse(&text)
            .map_err(|e| SeppError::Config(format!("pkg: plugins/{stem}.toml: {e}")))?;
        if m.name != stem {
            return Err(SeppError::Config(format!(
                "pkg: plugins/{stem}.toml hat name = {:?}, erwartet {stem:?}",
                m.name
            )));
        }
        plugin_manifests.insert(stem, m);
    }
    let inventory = manifest.inventory();
    Ok(InstallPlan {
        signed,
        inventory,
        previous,
        trust,
        plugin_manifests,
    })
}

/// Was einer Installation im Weg steht — Fehler brechen ab, Warnungen stehen im Dialog.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collisions {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Plugin-Namen gegen die Plugins des Nutzers und anderer Pakete (Fehler — sie teilen sich
/// die `[plugin.<name>]`-Gewährung), Prompts und Skills gegen dieselben (Warnung — der Loader
/// nimmt den ersten Treffer, und das ist der des Nutzers).
pub fn check_collisions(roots: &Roots, plan: &InstallPlan) -> Collisions {
    let mut c = Collisions::default();
    let own = roots.package_dir(plan.name());

    // Plugins des Nutzers: Name aus <stamm>.toml, sonst Stamm (wie der Loader).
    let user_plugins = plugin_names_in(&roots.user_plugins_dir());
    for stem in &plan.inventory.plugins {
        if user_plugins.contains(stem) {
            c.errors.push(format!(
                "Plugin `{stem}` gibt es schon unter {} — gleicher Name, gleiche Gewährung \
                 [plugin.{stem}]; eines von beiden umbenennen",
                roots.user_plugins_dir().display()
            ));
        }
    }
    let user_prompts = names_in(&roots.user_prompts_dir(), ".md");
    for p in &plan.inventory.prompts {
        if user_prompts.contains(p) {
            c.warnings.push(format!(
                "Prompt `/{p}` gibt es schon unter {} — der eigene gewinnt, der aus dem Paket \
                 bleibt unerreichbar",
                roots.user_prompts_dir().display()
            ));
        }
    }
    let user_skills = skill_names_in(&roots.user_skills_dir());
    for s in &plan.inventory.skills {
        if user_skills.contains(s) {
            c.warnings.push(format!(
                "Skill `{s}` gibt es schon unter {} — beide landen im System-Prompt",
                roots.user_skills_dir().display()
            ));
        }
    }

    for dir in roots.package_dirs() {
        if dir == own {
            continue;
        }
        let other = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let plugins = plugin_names_in(&dir.join("plugins"));
        for stem in &plan.inventory.plugins {
            if plugins.contains(stem) {
                c.errors.push(format!(
                    "Plugin `{stem}` ist schon durch Paket `{other}` installiert"
                ));
            }
        }
        let prompts = names_in(&dir.join("prompts"), ".md");
        for p in &plan.inventory.prompts {
            if prompts.contains(p) {
                c.warnings.push(format!(
                    "Prompt `/{p}` kommt auch aus Paket `{other}` — nur einer ist erreichbar"
                ));
            }
        }
        let skills = skill_names_in(&dir.join("skills"));
        for s in &plan.inventory.skills {
            if skills.contains(s) {
                c.warnings.push(format!(
                    "Skill `{s}` kommt auch aus Paket `{other}` — beide landen im System-Prompt"
                ));
            }
        }
    }
    c
}

fn names_in(dir: &Path, suffix: &str) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_suffix(suffix).map(|s| s.to_string())
        })
        .collect()
}

fn skill_names_in(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            if e.path().is_dir() {
                e.path().join("SKILL.md").is_file().then_some(n)
            } else {
                n.strip_suffix(".md").map(|s| s.to_string())
            }
        })
        .collect()
}

/// Plugin-Namen in einem Verzeichnis: aus `<stamm>.toml` (`name`), sonst der Stamm.
fn plugin_names_in(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("wasm") {
                return None;
            }
            let stem = path.file_stem()?.to_str()?.to_string();
            let toml = path.with_extension("toml");
            Some(Manifest::from_file(&toml).map(|m| m.name).unwrap_or(stem))
        })
        .collect()
}

/// Prüft die aufgelösten Rechte gegen die Selbstauskunft der Plugins. Fehler: das Paket bittet
/// um eine **Art** von Zugriff, die das Plugin-Manifest nicht deklariert (fremder Host, fremde
/// Variable, Datei ohne fs-Recht) — dann wäre die Gewährung wirkungslos oder das Paket lügt.
/// Warnung: ein Pfadrecht, das der Manifest-Präfix nicht deckt (der Schnitt wäre leer, weil
/// Manifest-Pfade zur Laufzeit gegen das Arbeitsverzeichnis aufgelöst werden).
pub fn check_rights(
    plan: &InstallPlan,
    rights: &[(String, Grants)],
    ctx: &ResolveCtx,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    for (plugin, grants) in rights {
        let Some(m) = plan.plugin_manifests.get(plugin) else {
            return Err(SeppError::Config(format!(
                "pkg: [rights.{plugin}] ohne plugins/{plugin}.toml"
            )));
        };
        let declared = m.capabilities.to_policy_with(ctx);
        let declares_read =
            !m.capabilities.fs_read.is_empty() || !m.capabilities.fs_write.is_empty();
        let declares_write = !m.capabilities.fs_write.is_empty();
        let requested: Policy = grants.to_policy_with(ctx);
        for cap in &requested.granted {
            match cap {
                Capability::Net { host } => {
                    if !declared.allows(cap) {
                        return Err(SeppError::Config(format!(
                            "pkg: [rights.{plugin}] bittet um net = {host:?}, aber das \
                             Plugin-Manifest deklariert diesen Host nicht"
                        )));
                    }
                }
                Capability::Env { name } => {
                    if !declared.allows(cap) {
                        return Err(SeppError::Config(format!(
                            "pkg: [rights.{plugin}] bittet um env = {name:?}, aber das \
                             Plugin-Manifest deklariert diese Variable nicht"
                        )));
                    }
                }
                Capability::FsRead { prefix } => {
                    if !declares_read {
                        return Err(SeppError::Config(format!(
                            "pkg: [rights.{plugin}] bittet um fs_read, aber das Plugin-Manifest \
                             deklariert keinen Dateizugriff"
                        )));
                    }
                    if !declared.allows(cap) {
                        warnings.push(format!(
                            "Plugin {plugin}: fs_read {} liegt außerhalb der Pfade, die das \
                             Plugin-Manifest nennt — wirksam ist nur der Schnitt, und der wäre \
                             leer, solange sepp nicht aus einem passenden Verzeichnis startet",
                            prefix.display()
                        ));
                    }
                }
                Capability::FsWrite { prefix } => {
                    if !declares_write {
                        return Err(SeppError::Config(format!(
                            "pkg: [rights.{plugin}] bittet um fs_write, aber das Plugin-Manifest \
                             deklariert kein Schreibrecht"
                        )));
                    }
                    if !declared.allows(cap) {
                        warnings.push(format!(
                            "Plugin {plugin}: fs_write {} liegt außerhalb der Pfade, die das \
                             Plugin-Manifest nennt — wirksam ist nur der Schnitt",
                            prefix.display()
                        ));
                    }
                }
                Capability::Exec { .. } => {
                    return Err(SeppError::Config(format!(
                        "pkg: [rights.{plugin}]: exec ist für Plugins unzulässig"
                    )))
                }
            }
        }
    }
    Ok(warnings)
}

/// Der Text, dem der Nutzer zustimmt — rein, damit Dialog und Fehlerfall denselben zeigen.
pub fn consent_lines(
    plan: &InstallPlan,
    rights: &[(String, Grants)],
    values: &BTreeMap<String, String>,
    warnings: &[String],
) -> Vec<String> {
    let m = plan.manifest();
    let mut out = Vec::new();
    let desc = m
        .description
        .as_deref()
        .map(|d| format!(" — {d}"))
        .unwrap_or_default();
    out.push(format!("Paket {} {}{desc}", m.name, m.version));
    if let Some(prev) = &plan.previous {
        out.push(format!("  Upgrade von {}", prev.version));
    }
    let trust = match &plan.trust {
        TrustStatus::Known => "bekannt".to_string(),
        TrustStatus::New { .. } => "NEU — Fingerprint prüfen".to_string(),
        TrustStatus::Mismatch { stored, .. } => {
            format!("ANDERER SCHLÜSSEL als gespeichert ({stored})")
        }
    };
    out.push(format!(
        "  Herausgeber {} · Schlüssel {} ({trust})",
        m.publisher.name, plan.signed.fingerprint
    ));
    let inv = &plan.inventory;
    out.push(format!(
        "  Inhalt: {} Skills, {} Prompts, {} Hooks, {} Plugins",
        inv.skills.len(),
        inv.prompts.len(),
        inv.hooks.len(),
        inv.plugins.len()
    ));
    if !values.is_empty() {
        for (k, v) in values {
            out.push(format!("  {k} = {v}"));
        }
    }
    for stem in &inv.plugins {
        match rights.iter().find(|(p, _)| p == stem) {
            Some((_, g)) if !g.is_empty() => {
                out.push(format!("  Plugin {stem} bittet um:"));
                for p in &g.fs_read {
                    out.push(format!("    fs_read  {p}"));
                }
                for p in &g.fs_write {
                    out.push(format!("    fs_write {p}"));
                }
                match &g.net {
                    NetGrant::Off => {}
                    NetGrant::All => out.push("    net      jeder Host".to_string()),
                    NetGrant::Hosts(h) => {
                        for host in h {
                            out.push(format!("    net      {host}"));
                        }
                    }
                }
                for e in &g.env {
                    out.push(format!(
                        "    env      {e} (Secret, wird im Host eingesetzt)"
                    ));
                }
            }
            _ => out.push(format!("  Plugin {stem}: keine Rechte")),
        }
    }
    for h in &inv.hooks {
        out.push(format!(
            "  Hook {h}: läuft im Agent-Loop — kann jeden Werkzeugaufruf blockieren und \
             Eingaben umschreiben"
        ));
    }
    for w in warnings {
        out.push(format!("  Hinweis: {w}"));
    }
    out
}

/// Ergebnis von [`apply_install`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub name: String,
    pub version: String,
    pub dir: PathBuf,
    pub files: usize,
    /// Ob ein Policy-Block geschrieben wurde (nur bei Rechten).
    pub policy_written: bool,
    pub policy_path: PathBuf,
    pub upgraded_from: Option<String>,
}

/// Führt die Installation aus: Staging → Umbenennen → Policy-Block → Nachweis. Bei einem Fehler
/// nach dem Umbenennen wird das Verzeichnis zurückgerollt.
pub fn apply_install(
    roots: &Roots,
    archive: &PkgArchive,
    plan: &InstallPlan,
    values: &BTreeMap<String, String>,
    rights: &[(String, Grants)],
) -> Result<InstallReport> {
    let name = plan.name().to_string();
    let version = plan.manifest().version.clone();
    std::fs::create_dir_all(roots.pkg_dir())
        .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", roots.pkg_dir().display())))?;

    let pid = std::process::id();
    let staging = roots.pkg_dir().join(format!(".staging-{name}-{pid}"));
    let _ = std::fs::remove_dir_all(&staging);
    let report = match archive.extract_verified(&plan.signed, &staging) {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    let target = roots.package_dir(&name);
    let old = roots.pkg_dir().join(format!(".old-{name}-{pid}"));
    let had_old = target.exists();
    if had_old {
        let _ = std::fs::remove_dir_all(&old);
        std::fs::rename(&target, &old).map_err(|e| {
            let _ = std::fs::remove_dir_all(&staging);
            SeppError::Config(format!("pkg: {}: {e}", target.display()))
        })?;
    }
    if let Err(e) = std::fs::rename(&staging, &target) {
        let _ = std::fs::remove_dir_all(&staging);
        if had_old {
            let _ = std::fs::rename(&old, &target);
        }
        return Err(SeppError::Config(format!("pkg: {}: {e}", target.display())));
    }

    let actor_grants: Vec<(Actor, Grants)> = rights
        .iter()
        .map(|(p, g)| (Actor::Plugin(p.clone()), g.clone()))
        .collect();
    let policy_written = actor_grants.iter().any(|(_, g)| !g.is_empty());
    if let Err(e) = write_package_section(&roots.policy_path(), &name, &version, &actor_grants) {
        // Zurückrollen: neues Verzeichnis weg, altes zurück.
        let _ = std::fs::remove_dir_all(&target);
        if had_old {
            let _ = std::fs::rename(&old, &target);
        }
        return Err(e);
    }
    if had_old {
        let _ = std::fs::remove_dir_all(&old);
    }

    let mut installed = Installed::load(roots)?;
    installed.packages.insert(
        name.clone(),
        InstalledEntry {
            version: version.clone(),
            installed_at: now(),
            publisher: plan.manifest().publisher.name.clone(),
            publisher_fp: plan.signed.fingerprint.clone(),
            vars: values.clone(),
            files: report.files,
            plugins: plan.inventory.plugins.clone(),
            hooks: plan.inventory.hooks.clone(),
        },
    );
    installed.save(roots)?;

    Ok(InstallReport {
        name,
        version,
        dir: target,
        files: report.files,
        policy_written,
        policy_path: roots.policy_path(),
        upgraded_from: plan.previous.as_ref().map(|p| p.version.clone()),
    })
}

/// Ergebnis von [`remove`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveReport {
    pub name: String,
    pub policy_removed: bool,
    pub dir_removed: bool,
    pub receipt_removed: bool,
    pub publisher: Option<String>,
}

/// Entfernt Policy-Block, Verzeichnis und Nachweis. Die Dateien des Nutzers und das Vertrauen in
/// den Herausgeber bleiben. Nichts von alledem vorhanden → Fehler.
pub fn remove(roots: &Roots, name: &str) -> Result<RemoveReport> {
    crate::validate_name(name)?;
    let policy_removed = remove_package_section(&roots.policy_path(), name)?;
    let dir = roots.package_dir(name);
    let dir_removed = if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", dir.display())))?;
        true
    } else {
        false
    };
    let mut installed = Installed::load(roots)?;
    let entry = installed.packages.remove(name);
    let receipt_removed = entry.is_some();
    if receipt_removed {
        installed.save(roots)?;
    }
    if !policy_removed && !dir_removed && !receipt_removed {
        return Err(SeppError::Config(format!(
            "pkg: {name} ist nicht installiert (weder Verzeichnis, Policy-Block noch Nachweis)"
        )));
    }
    Ok(RemoveReport {
        name: name.to_string(),
        policy_removed,
        dir_removed,
        receipt_removed,
        publisher: entry.map(|e| e.publisher),
    })
}

/// Ein Paket, wie `sepp pkg list` es zeigt — aus Nachweis und Verzeichnis zusammengesetzt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub receipt: Option<InstalledEntry>,
    pub dir_present: bool,
}

/// Alle Pakete: die mit Nachweis und die, deren Verzeichnis ohne Nachweis daliegt.
pub fn list(roots: &Roots) -> Result<Vec<Listed>> {
    let installed = Installed::load(roots)?;
    let mut names: Vec<String> = installed.packages.keys().cloned().collect();
    for dir in roots.package_dirs() {
        if let Some(n) = dir.file_name().and_then(|n| n.to_str()) {
            if !names.iter().any(|x| x == n) {
                names.push(n.to_string());
            }
        }
    }
    names.sort();
    Ok(names
        .into_iter()
        .map(|name| Listed {
            dir_present: roots.package_dir(&name).is_dir(),
            receipt: installed.packages.get(&name).cloned(),
            name,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_dirs_skip_hidden_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pkg");
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join(".staging-x-1")).unwrap();
        std::fs::create_dir_all(root.join(".old-x-1")).unwrap();
        std::fs::write(root.join("datei"), "").unwrap();
        let dirs = package_dirs_in(&root);
        assert_eq!(dirs, vec![root.join("a"), root.join("b")]);
        assert!(package_dirs_in(&tmp.path().join("fehlt")).is_empty());
    }

    #[test]
    fn installed_json_round_trips_with_private_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = Roots {
            config: tmp.path().join("c"),
            state: tmp.path().join("s"),
        };
        let mut i = Installed::load(&roots).unwrap();
        assert!(i.packages.is_empty());
        i.packages.insert(
            "demo".into(),
            InstalledEntry {
                version: "1.0.0".into(),
                installed_at: 1,
                publisher: "acme".into(),
                publisher_fp: "ab".into(),
                vars: BTreeMap::new(),
                files: 3,
                plugins: vec!["z".into()],
                hooks: vec![],
            },
        );
        i.save(&roots).unwrap();
        assert_eq!(Installed::load(&roots).unwrap(), i);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(roots.installed_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(m, 0o600);
            let d = std::fs::metadata(roots.state_pkg_dir())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(d, 0o700);
        }
    }
}
