//! `sepp pkg keygen | pack | install | search | list | remove | untrust | index` — Pakete bauen,
//! finden, installieren; eine Registry betreiben.
//!
//! Die Logik liegt in `sepp-pkg` und arbeitet gegen abstrakte Wurzeln; hier stehen Parsing,
//! Pfade aus `session`, und die Dialoge. Alles, was der Nutzer beantworten muss (Vertrauen in
//! einen neuen Herausgeber, Variablen, die Zustimmung zu den Rechten), läuft über **stderr** und
//! stdin — stdout bleibt der Datenkanal und bekommt nur die Abschlussmeldung. Nicht-interaktiv
//! (Pipe, Skript) gibt es keine Fragen: Dann müssen `--yes`, `--trust-key` und `--var` alles
//! vorwegnehmen, sonst bricht `install` mit der Liste ab, der man zugestimmt hätte.
//!
//! `install <name>` und `search` holen Index und Paket aus einer Registry (`[[registries]]` in der
//! globalen `settings.toml`) über den HTTP-Fetcher in `pkg_fetch` — der bringt seine eigene
//! Runtime mit; hier gibt es keine. Der Kern (`install_file`, `install_from_registry`, `search`)
//! nimmt Wurzeln, Konfiguration und Fetcher als Parameter, damit Tests ohne Umgebung laufen.
//!
//! Wie die anderen Unterbefehle: `parse_*` ist rein, `run_*` liefert den `ExitCode`, innen
//! `anyhow` und eine zurückgegebene Meldung.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};

use sepp_core::{sanitize_display, sanitize_display_multiline};
use sepp_pkg::crypto::decode_pubkey;
use sepp_pkg::registry::{INDEX_FILE, SIG_FILE};
use sepp_pkg::{
    apply_install, build_index, check_collisions, check_rights, consent_lines, download_package,
    fetch_index, fingerprint, join_url, list, load_registries, pack_dir, parse_spec, plan_install,
    remove, resolve_rights, resolve_vars, trust_publisher, trusted_publishers, untrust_publisher,
    validate_name, value_notes, Fetcher, IndexEntry, Installed, PkgArchive, PkgSpec,
    RegistryConfig, Roots, TrustStatus, EXTENSION,
};
use sepp_policy::ResolveCtx;

use crate::pkg_fetch::HttpFetcher;
use crate::session;

/// Fehlertext ohne die Präfixe `config error: ` und `pkg: `, mit dem Registry-Namen genau einmal —
/// `fetch_index` nennt ihn schon, `resolve` nicht.
fn plain(name: &str, e: &sepp_core::SeppError) -> String {
    // Der Text stammt aus einer fremden Antwort (HTTP-Fehler, Index-Inhalt) und geht direkt
    // ans Terminal — Steuerzeichen darin würden Zeilen überschreiben oder färben.
    let s = sanitize_display(&e.to_string());
    let s = s.strip_prefix("config error: ").unwrap_or(s.as_str());
    let s = s.strip_prefix("pkg: ").unwrap_or(s);
    if s.starts_with("Registry »") {
        s.to_string()
    } else {
        format!("Registry »{name}«: {s}")
    }
}

/// Woher `install` sein Paket nimmt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallSource {
    File(PathBuf),
    Registry {
        spec: PkgSpec,
        /// `--registry <name>`: nur diese; sonst alle in Konfigurationsreihenfolge.
        registry: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkgCmd {
    /// Schlüsselpaar unter `<state_root>/pkg/` anlegen — für Herausgeber; mit `--registry` das
    /// Betreiber-Paar, das `index` signiert.
    Keygen {
        registry: bool,
    },
    /// Verzeichnis zu `<name>-<version>.seppkg` packen und signieren.
    Pack {
        dir: PathBuf,
        out: Option<PathBuf>,
        key: Option<PathBuf>,
    },
    Install {
        source: InstallSource,
        yes: bool,
        trust_key: Option<String>,
        vars: Vec<(String, String)>,
    },
    /// Den Index einer oder aller Registries durchsuchen.
    Search {
        text: Option<String>,
        registry: Option<String>,
    },
    List,
    Remove {
        name: String,
    },
    /// Vertrauen in einen Herausgeber zurücknehmen (`trusted-keys/<name>.json` löschen).
    Untrust {
        name: String,
    },
    /// Betreiber: `index.toml` + `index.sig` aus den `.seppkg`-Dateien eines Verzeichnisses.
    Index {
        dir: PathBuf,
        out: Option<PathBuf>,
        name: Option<String>,
        base_url: Option<String>,
        key: Option<PathBuf>,
    },
}

const USAGE: &str = "Verwendung: sepp pkg keygen [--registry]\n\
                     \x20           sepp pkg pack <dir> [--out <datei>] [--key <pfad>]\n\
                     \x20           sepp pkg install <datei.seppkg | name[@version]> [--registry <name>] [--yes] [--trust-key <fingerprint>] [--var NAME=WERT]…\n\
                     \x20           sepp pkg search [text] [--registry <name>]\n\
                     \x20           sepp pkg list\n\
                     \x20           sepp pkg remove <name>\n\
                     \x20           sepp pkg untrust <herausgeber>\n\
                     \x20           sepp pkg index <dir> [--out <dir>] [--name <name>] [--base-url <url>] [--key <pfad>]";

/// Zerlegt die Argumente hinter `sepp pkg`. Reine Funktion.
pub fn parse_pkg_args(args: &[String]) -> std::result::Result<PkgCmd, String> {
    let Some(sub) = args.first().map(String::as_str) else {
        return Err(format!("pkg: Unterbefehl fehlt\n{USAGE}"));
    };
    let rest = &args[1..];
    match sub {
        "keygen" => {
            let mut registry = false;
            for a in rest {
                match a.as_str() {
                    "--registry" => registry = true,
                    other => {
                        return Err(format!(
                            "pkg keygen: unerwartetes Argument '{other}'\n{USAGE}"
                        ))
                    }
                }
            }
            Ok(PkgCmd::Keygen { registry })
        }
        "list" => {
            reject_extra(rest, "list")?;
            Ok(PkgCmd::List)
        }
        "remove" => Ok(PkgCmd::Remove {
            name: single_name(rest, "remove")?,
        }),
        "untrust" => Ok(PkgCmd::Untrust {
            name: single_name(rest, "untrust")?,
        }),
        "index" => {
            let mut dir = None;
            let mut out = None;
            let mut name = None;
            let mut base_url = None;
            let mut key = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--out" => out = Some(PathBuf::from(value(&mut it, "--out")?)),
                    "--name" => name = Some(value(&mut it, "--name")?),
                    "--base-url" => base_url = Some(value(&mut it, "--base-url")?),
                    "--key" => key = Some(PathBuf::from(value(&mut it, "--key")?)),
                    other if other.starts_with('-') => {
                        return Err(format!("pkg index: unbekannte Option '{other}'\n{USAGE}"))
                    }
                    other if dir.is_none() => dir = Some(PathBuf::from(other)),
                    other => {
                        return Err(format!(
                            "pkg index: unerwartetes Argument '{other}'\n{USAGE}"
                        ))
                    }
                }
            }
            let dir = dir.ok_or_else(|| format!("pkg index: Verzeichnis fehlt\n{USAGE}"))?;
            if let Some(n) = &name {
                validate_name(n).map_err(|e| format!("pkg index: --name: {e}"))?;
            }
            Ok(PkgCmd::Index {
                dir,
                out,
                name,
                base_url,
                key,
            })
        }
        "pack" => {
            let mut dir = None;
            let mut out = None;
            let mut key = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--out" => out = Some(PathBuf::from(value(&mut it, "--out")?)),
                    "--key" => key = Some(PathBuf::from(value(&mut it, "--key")?)),
                    other if other.starts_with('-') => {
                        return Err(format!("pkg pack: unbekannte Option '{other}'\n{USAGE}"))
                    }
                    other if dir.is_none() => dir = Some(PathBuf::from(other)),
                    other => {
                        return Err(format!(
                            "pkg pack: unerwartetes Argument '{other}'\n{USAGE}"
                        ))
                    }
                }
            }
            let dir = dir.ok_or_else(|| format!("pkg pack: Verzeichnis fehlt\n{USAGE}"))?;
            Ok(PkgCmd::Pack { dir, out, key })
        }
        "install" => {
            let mut target = None;
            let mut registry = None;
            let mut yes = false;
            let mut trust_key = None;
            let mut vars = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--yes" | "-y" => yes = true,
                    "--trust-key" => trust_key = Some(value(&mut it, "--trust-key")?),
                    "--registry" => registry = Some(value(&mut it, "--registry")?),
                    "--var" => {
                        let v = value(&mut it, "--var")?;
                        let (k, val) = v.split_once('=').ok_or_else(|| {
                            format!("pkg install: --var erwartet NAME=WERT, nicht '{v}'\n{USAGE}")
                        })?;
                        if k.is_empty() {
                            return Err(format!("pkg install: --var ohne Namen\n{USAGE}"));
                        }
                        vars.push((k.to_string(), val.to_string()));
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("pkg install: unbekannte Option '{other}'\n{USAGE}"))
                    }
                    other if target.is_none() => target = Some(other.to_string()),
                    other => {
                        return Err(format!(
                            "pkg install: unerwartetes Argument '{other}'\n{USAGE}"
                        ))
                    }
                }
            }
            let target = target
                .ok_or_else(|| format!("pkg install: Datei oder Paketname fehlt\n{USAGE}"))?;
            let source = install_source(&target, registry)?;
            Ok(PkgCmd::Install {
                source,
                yes,
                trust_key,
                vars,
            })
        }
        "search" => {
            let mut text = None;
            let mut registry = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--registry" => registry = Some(value(&mut it, "--registry")?),
                    other if other.starts_with('-') => {
                        return Err(format!("pkg search: unbekannte Option '{other}'\n{USAGE}"))
                    }
                    other if text.is_none() => text = Some(other.to_string()),
                    other => {
                        return Err(format!(
                            "pkg search: unerwartetes Argument '{other}'\n{USAGE}"
                        ))
                    }
                }
            }
            if let Some(r) = &registry {
                validate_name(r).map_err(|e| format!("pkg search: --registry: {e}"))?;
            }
            Ok(PkgCmd::Search { text, registry })
        }
        other => Err(format!(
            "pkg: unbekannter Unterbefehl '{other}' (erlaubt: keygen, pack, install, search, list, \
             remove, untrust, index)\n{USAGE}"
        )),
    }
}

/// Datei oder Paketname — rein syntaktisch: Endung `.seppkg`, ein Pfadtrenner oder ein
/// führender `.` machen eine Datei, sonst gilt `name[@version]`. Wer eine Datei ohne Endung
/// meint, schreibt `./name`.
fn install_source(
    target: &str,
    registry: Option<String>,
) -> std::result::Result<InstallSource, String> {
    let looks_like_file = target.ends_with(&format!(".{EXTENSION}"))
        || target.contains('/')
        || target.contains('\\')
        || target.starts_with('.');
    if looks_like_file {
        if registry.is_some() {
            return Err(format!(
                "pkg install: --registry gilt für `install <name>`, nicht für eine Datei\n{USAGE}"
            ));
        }
        return Ok(InstallSource::File(PathBuf::from(target)));
    }
    let spec = parse_spec(target).map_err(|e| {
        format!(
            "pkg install: {target:?} ist weder eine .seppkg-Datei noch ein Paketname ({e}) — \
             eine Datei ohne Endung als ./{target} angeben\n{USAGE}"
        )
    })?;
    if let Some(r) = &registry {
        validate_name(r).map_err(|e| format!("pkg install: --registry: {e}"))?;
    }
    Ok(InstallSource::Registry { spec, registry })
}

/// Genau ein Positionsargument, keine Optionen (`remove <name>`, `untrust <herausgeber>`).
fn single_name(rest: &[String], sub: &str) -> std::result::Result<String, String> {
    let mut name = None;
    for a in rest {
        if a.starts_with('-') {
            return Err(format!("pkg {sub}: unbekannte Option '{a}'\n{USAGE}"));
        }
        if name.is_some() {
            return Err(format!("pkg {sub}: unerwartetes Argument '{a}'\n{USAGE}"));
        }
        name = Some(a.clone());
    }
    name.ok_or_else(|| format!("pkg {sub}: Name fehlt\n{USAGE}"))
}

fn value(it: &mut std::slice::Iter<'_, String>, flag: &str) -> std::result::Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("pkg: {flag} braucht einen Wert\n{USAGE}"))
}

fn reject_extra(rest: &[String], sub: &str) -> std::result::Result<(), String> {
    match rest.first() {
        Some(a) => Err(format!("pkg {sub}: unerwartetes Argument '{a}'\n{USAGE}")),
        None => Ok(()),
    }
}

pub fn run_pkg(cmd: PkgCmd) -> ExitCode {
    let result = match cmd {
        PkgCmd::Keygen { registry } => keygen(registry),
        PkgCmd::Pack { dir, out, key } => pack(&dir, out.as_deref(), key.as_deref()),
        PkgCmd::Install {
            source,
            yes,
            trust_key,
            vars,
        } => install_cmd(
            source,
            &Answers {
                yes,
                trust_key: trust_key.as_deref(),
                vars: &vars,
            },
        ),
        PkgCmd::Search { text, registry } => search_cmd(text.as_deref(), registry.as_deref()),
        PkgCmd::List => list_cmd(),
        PkgCmd::Remove { name } => remove_cmd(&name),
        PkgCmd::Untrust { name } => untrust_cmd(&name),
        PkgCmd::Index {
            dir,
            out,
            name,
            base_url,
            key,
        } => index_cmd(
            &dir,
            out.as_deref(),
            name.as_deref(),
            base_url.as_deref(),
            key.as_deref(),
        ),
    };
    // Beide Ausgänge bereinigt: Der Erfolgstext trägt Tabellen mit Beschreibungen aus einem
    // Registry-Index, der Fehlertext die Antwort eines fremden Servers. Mehrzeilig, deshalb
    // bleiben echte Umbrüche erhalten.
    match result {
        Ok(text) => {
            print!("{}", sanitize_display_multiline(&text));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Fehler: {}", sanitize_display_multiline(&e.to_string()));
            ExitCode::FAILURE
        }
    }
}

fn roots() -> Result<Roots> {
    Ok(Roots {
        config: session::config_root()?,
        state: session::state_root()?,
    })
}

/// Fragen gibt es nur, wenn jemand antworten kann — stdin **oder** das Terminal selbst.
///
/// `/dev/tty` ist der Grund für das „oder": `curl … | sh -s -- --uninstall --purge` ist ein
/// dokumentierter Weg, und dort ist stdin die Pipe. Ein Installer, der deshalb nicht fragt,
/// löscht ungefragt; einer, der abbricht, ist unbenutzbar. Also fragen wir das Terminal direkt.
pub(crate) fn interactive() -> bool {
    (std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) || tty().is_some()
}

/// Das steuernde Terminal, falls es eins gibt (nicht auf Windows).
fn tty() -> Option<std::fs::File> {
    #[cfg(unix)]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .open("/dev/tty")
            .ok()
            .filter(|f| f.is_terminal())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Stellt die Frage auf stderr und liest die Antwort — von stdin, wenn das ein Terminal ist,
/// sonst vom steuernden Terminal.
pub(crate) fn ask(prompt: &str, default: Option<&str>) -> Result<String> {
    let mut err = std::io::stderr();
    match default {
        Some(d) => write!(err, "{prompt} [{d}]: ")?,
        None => write!(err, "{prompt}: ")?,
    }
    err.flush()?;
    let mut line = String::new();
    if std::io::stdin().is_terminal() {
        std::io::stdin().read_line(&mut line)?;
    } else {
        let tty = tty().ok_or_else(|| anyhow!("kein Terminal für die Rückfrage"))?;
        std::io::BufRead::read_line(&mut std::io::BufReader::new(tty), &mut line)?;
    }
    let line = line.trim();
    if line.is_empty() {
        return Ok(default.unwrap_or("").to_string());
    }
    Ok(line.to_string())
}

/// Ja/Nein-Frage; alles außer einer ausdrücklichen Zustimmung ist Nein.
pub(crate) fn confirm(question: &str) -> Result<bool> {
    let a = ask(&format!("{question} [j/N]"), None)?;
    Ok(matches!(
        a.to_lowercase().as_str(),
        "j" | "ja" | "y" | "yes"
    ))
}

fn keygen(registry: bool) -> Result<String> {
    let roots = roots()?;
    let files = if registry {
        roots.registry_key_files()
    } else {
        roots.key_files()
    };
    let fp = sepp_pkg::crypto::write_new_keypair(&files)?;
    let mut text = format!(
        "Schlüsselpaar angelegt:\n  {}  (geheim, 0600 — nie weitergeben)\n  {}\n\
         Fingerprint {fp}\n\n",
        files.secret.display(),
        files.public.display()
    );
    if registry {
        let public = std::fs::read_to_string(&files.public)?.trim().to_string();
        text.push_str(&format!(
            "Damit signiert `sepp pkg index`. Nutzer tragen den Public Key in ihre settings.toml \
             ein:\n\n{}\n",
            registries_snippet("kionova", &public)
        ));
    } else {
        text.push_str(
            "Damit signiert `sepp pkg pack`. Nutzer sehen den Fingerprint beim ersten Paket und \
             bestätigen ihn einmal.\n",
        );
    }
    Ok(text)
}

/// Der fertige `[[registries]]`-Eintrag, den Nutzer übernehmen.
fn registries_snippet(name: &str, public_key: &str) -> String {
    format!(
        "[[registries]]\nname = \"{name}\"\nurl = \"https://<host>/<pfad>/{INDEX_FILE}\"\nkey = \"{public_key}\""
    )
}

fn pack(dir: &Path, out: Option<&Path>, key: Option<&Path>) -> Result<String> {
    let key_path = match key {
        Some(k) => k.to_path_buf(),
        None => roots()?.key_files().secret,
    };
    if !key_path.is_file() {
        bail!(
            "kein Signierschlüssel unter {} — erst `sepp pkg keygen` (oder --key <pfad>)",
            key_path.display()
        );
    }
    let key = sepp_pkg::crypto::load_signing_key(&key_path)?;
    // Zielname aus dem Manifest, bevor gepackt wird — damit ein Tippfehler dort früh auffällt.
    let manifest_text = std::fs::read_to_string(dir.join("manifest.toml"))
        .with_context(|| format!("{}: manifest.toml fehlt", dir.display()))?;
    let (name, version) = name_and_version(&manifest_text)?;
    let out = match out {
        Some(o) => o.to_path_buf(),
        None => PathBuf::from(format!("{name}-{version}.{EXTENSION}")),
    };
    let report = pack_dir(dir, &key, &out)?;
    let mut text = format!(
        "Paket geschrieben: {} ({} Dateien, {} Bytes, Herausgeber-Fingerprint {})\n",
        report.out.display(),
        report.files,
        report.bytes,
        report.fingerprint
    );
    for w in report.warnings {
        text.push_str(&format!("Hinweis: {w}\n"));
    }
    Ok(text)
}

/// `name` und `version` aus einem Manifest-Text, ohne ihn zu validieren (das macht `pack`).
fn name_and_version(text: &str) -> Result<(String, String)> {
    let doc: toml::Value = toml::from_str(text).context("manifest.toml")?;
    let get = |k: &str| {
        doc.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("manifest.toml: `{k}` fehlt"))
    };
    Ok((get("name")?, get("version")?))
}

/// Antworten, die der Nutzer vorwegnehmen kann — nicht-interaktiv muss er es.
pub(crate) struct Answers<'a> {
    pub yes: bool,
    pub trust_key: Option<&'a str>,
    pub vars: &'a [(String, String)],
}

/// Woher ein Paket kommt — für Nachweis, `TrustedKey.via` und den Zustimmungsdialog.
struct Origin {
    /// `datei` oder `registry:<name>`, landet im Nachweis.
    label: String,
    /// Text für `TrustedKey.via`.
    via: String,
    /// Zusätzliche Zeile im Zustimmungsdialog (Registry und aufgelöste Paket-URL).
    consent_line: Option<String>,
    /// Herausgeber-Schlüssel laut Index — muss zum Paket passen, sonst gibt es keinen Dialog.
    expected_publisher_key: Option<String>,
}

impl Origin {
    fn file(path: &Path) -> Self {
        Origin {
            label: "datei".into(),
            via: format!("install {}", path.display()),
            consent_line: None,
            expected_publisher_key: None,
        }
    }
}

/// Eine geladene Datei, die nach dem Versuch immer verschwindet — auch bei jedem `?`.
struct Downloaded(PathBuf);

impl Drop for Downloaded {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const NO_REGISTRY: &str = "keine Registry konfiguriert — in der globalen settings.toml einen \
                           Abschnitt [[registries]] mit name, url und key anlegen (Vorlage: \
                           `sepp init`)";

fn install_cmd(source: InstallSource, answers: &Answers) -> Result<String> {
    let roots = roots()?;
    match source {
        InstallSource::File(file) => install_file(&roots, &file, &Origin::file(&file), answers),
        InstallSource::Registry { spec, registry } => {
            let registries = load_registries(&session::registries_path()?)?;
            let fetcher = HttpFetcher::new()?;
            install_from_registry(
                &roots,
                &registries,
                &fetcher,
                &spec,
                registry.as_deref(),
                answers,
            )
        }
    }
}

/// Eine bestimmte Registry (`--registry`) oder alle in Konfigurationsreihenfolge.
fn pick_registries<'a>(
    registries: &'a [RegistryConfig],
    which: Option<&str>,
) -> Result<Vec<&'a RegistryConfig>> {
    if registries.is_empty() {
        bail!("{NO_REGISTRY}");
    }
    match which {
        Some(w) => {
            let cfg = registries.iter().find(|r| r.name == w).ok_or_else(|| {
                anyhow!(
                    "Registry »{w}« ist nicht konfiguriert (bekannt: {})",
                    registries
                        .iter()
                        .map(|r| r.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            Ok(vec![cfg])
        }
        None => Ok(registries.iter().collect()),
    }
}

/// `install <name>[@version]`: Index holen, Eintrag auflösen, Paket nach `downloads/` laden
/// (gedeckelt, gehasht), dann derselbe Weg wie bei einer Datei. Die geladene Datei verschwindet
/// danach in jedem Fall.
pub(crate) fn install_from_registry(
    roots: &Roots,
    registries: &[RegistryConfig],
    fetcher: &dyn Fetcher,
    spec: &PkgSpec,
    which: Option<&str>,
    answers: &Answers,
) -> Result<String> {
    let wanted = match &spec.version {
        Some(v) => format!("{} {v}", spec.name),
        None => spec.name.clone(),
    };
    let mut failures: Vec<String> = Vec::new();
    let mut found: Option<(&RegistryConfig, IndexEntry)> = None;
    for cfg in pick_registries(registries, which)? {
        let index = match fetch_index(fetcher, cfg) {
            Ok(i) => i,
            Err(e) if which.is_some() => return Err(e.into()),
            Err(e) => {
                eprintln!("Hinweis: übersprungen — {}", plain(&cfg.name, &e));
                failures.push(plain(&cfg.name, &e));
                continue;
            }
        };
        let unknown = index.unknown_keys();
        if !unknown.is_empty() {
            eprintln!(
                "Hinweis: unbekannte Felder im Index von »{}«, ohne Wirkung: {}",
                cfg.name,
                sanitize_display(&unknown.join(", "))
            );
        }
        match index.resolve(&spec.name, spec.version.as_deref()) {
            Ok(e) => {
                found = Some((cfg, e.clone()));
                break;
            }
            Err(e) if which.is_some() => return Err(e.into()),
            Err(e) => failures.push(plain(&cfg.name, &e)),
        }
    }
    let Some((cfg, entry)) = found else {
        bail!("{wanted} nicht gefunden:\n  {}", failures.join("\n  "));
    };
    let url = join_url(&cfg.url, &entry.url)?;
    eprintln!(
        "Lade {} {} aus Registry »{}« ({} Bytes) …",
        entry.name, entry.version, cfg.name, entry.size
    );
    let path = download_package(fetcher, &cfg.url, &entry, &roots.downloads_dir())?;
    let _cleanup = Downloaded(path.clone());
    let origin = Origin {
        label: format!("registry:{}", cfg.name),
        via: format!("install {} aus Registry {}", spec.name, cfg.name),
        consent_line: Some(format!("  Quelle: Registry »{}« · {url}", cfg.name)),
        expected_publisher_key: Some(entry.publisher_key.clone()),
    };
    install_file(roots, &path, &origin, answers)
}

/// Der Weg von Stufe 4, für Datei und Registry gleich: prüfen, vertrauen, Variablen, Rechte,
/// Kollisionen, Zustimmung, anwenden.
fn install_file(roots: &Roots, file: &Path, origin: &Origin, answers: &Answers) -> Result<String> {
    let ctx = ResolveCtx::from_env();
    let archive = PkgArchive::open(file)?;
    let mut plan = plan_install(roots, &archive)?;
    plan.source = Some(origin.label.clone());
    let manifest = plan.manifest();
    if let Some(expected) = &origin.expected_publisher_key {
        if expected.trim() != manifest.publisher.key.trim() {
            bail!(
                "Index und Paket widersprechen sich: für Herausgeber »{}« nennt der Index den \
                 Schlüssel {}, das Paket ist mit {} signiert — nichts installiert",
                manifest.publisher.name,
                fingerprint(&decode_pubkey(expected)?),
                plan.signed.fingerprint
            );
        }
    }
    let unknown = manifest.unknown_keys();
    if !unknown.is_empty() {
        eprintln!(
            "Hinweis: unbekannte Felder im Paket-Manifest, ohne Wirkung: {}",
            sanitize_display(&unknown.join(", "))
        );
    }

    // 1 · Vertrauen in den Herausgeber.
    match &plan.trust {
        TrustStatus::Known => {}
        TrustStatus::Mismatch {
            name,
            stored,
            offered,
        } => bail!(
            "Herausgeber {name} ist mit Schlüssel {stored} bekannt, dieses Paket ist mit {offered} \
             signiert. Das wird nie stillschweigend akzeptiert. Wenn der Herausgeber den Schlüssel \
             wirklich gewechselt hat: `sepp pkg untrust {name}` und neu installieren."
        ),
        TrustStatus::New { name, fingerprint } => {
            let accepted = match answers.trust_key {
                Some(given) if given.eq_ignore_ascii_case(fingerprint) => true,
                Some(given) => bail!(
                    "--trust-key {given} passt nicht zum Schlüssel des Pakets ({fingerprint})"
                ),
                None if interactive() => {
                    eprintln!(
                        "Herausgeber »{name}« ist neu. Schlüssel-Fingerprint: {fingerprint}\n\
                         Prüfe ihn gegen eine zweite Quelle (Website, Rechnung, Telefon), bevor du \
                         vertraust — danach müssen alle Pakete von »{name}« zu diesem Schlüssel passen."
                    );
                    confirm(&format!(
                        "Herausgeber »{name}« mit Fingerprint {fingerprint} vertrauen?"
                    ))?
                }
                None => bail!(
                    "Herausgeber »{name}« ist neu (Fingerprint {fingerprint}). Nicht-interaktiv: \
                     `--trust-key {fingerprint}` angeben, wenn der Fingerprint geprüft ist."
                ),
            };
            if !accepted {
                bail!("abgebrochen — Herausgeber nicht vertraut");
            }
            trust_publisher(roots, &manifest.publisher, &origin.via)?;
        }
    }

    // 2 · Variablen.
    let given_map: BTreeMap<String, String> = answers.vars.iter().cloned().collect();
    let previous_vars = plan.previous.as_ref().map(|p| &p.vars);
    let mut resolved = resolve_vars(manifest, &given_map, previous_vars)?;
    if !resolved.missing.is_empty() {
        if !interactive() {
            let names: Vec<String> = resolved
                .missing
                .iter()
                .map(|(n, spec)| format!("  --var {n}=…   ({})", spec.description))
                .collect();
            bail!(
                "das Paket braucht Angaben, die nicht-interaktiv fehlen:\n{}",
                names.join("\n")
            );
        }
        let missing = std::mem::take(&mut resolved.missing);
        for (name, spec) in missing {
            let v = ask(
                &format!("{name} ({})", spec.description),
                spec.default.as_deref(),
            )?;
            if v.trim().is_empty() {
                bail!("{name} braucht einen Wert");
            }
            resolved.values.insert(name, v);
        }
    }
    let values = resolved.values;
    let notes = value_notes(manifest, &values, &ctx);

    // 3 · Rechte und Kollisionen.
    let rights = resolve_rights(manifest, &values, &ctx)?;
    let mut warnings = check_rights(&plan, &rights, &ctx)?;
    warnings.extend(notes);
    let collisions = check_collisions(roots, &plan);
    if !collisions.errors.is_empty() {
        bail!(
            "Installation nicht möglich:\n  {}",
            collisions.errors.join("\n  ")
        );
    }
    warnings.extend(collisions.warnings);

    // 4 · Zustimmung.
    let mut lines = consent_lines(&plan, &rights, &values, &warnings);
    if let Some(l) = &origin.consent_line {
        lines.insert(1.min(lines.len()), l.clone());
    }
    eprintln!("{}", lines.join("\n"));
    let approved = if answers.yes {
        true
    } else if interactive() {
        confirm("Installieren und diese Rechte gewähren?")?
    } else {
        bail!(
            "nicht-interaktiv: `--yes` angeben, um dem oben Gezeigten zuzustimmen (Rechte werden in \
             {} geschrieben)",
            roots.policy_path().display()
        );
    };
    if !approved {
        bail!("abgebrochen — nichts installiert");
    }

    // 5 · Anwenden.
    let report = apply_install(roots, &archive, &plan, &values, &rights)?;
    let mut text = format!(
        "{} {} installiert nach {}",
        report.name,
        report.version,
        report.dir.display()
    );
    if let Some(from) = &report.upgraded_from {
        text.push_str(&format!(" (Upgrade von {from})"));
    }
    text.push('\n');
    if report.policy_written {
        text.push_str(&format!(
            "Rechte als Block `# von sepp pkg: {}` in {} eingetragen.\n",
            report.name,
            report.policy_path.display()
        ));
    }
    text.push_str(
        "sepp neu starten, damit Plugins geladen werden. Kontrolle: `sepp policy` · `sepp pkg list` · \
         Entfernen: `sepp pkg remove ",
    );
    text.push_str(&report.name);
    text.push_str("`\n");
    Ok(text)
}

fn search_cmd(text: Option<&str>, which: Option<&str>) -> Result<String> {
    let roots = roots()?;
    let registries = load_registries(&session::registries_path()?)?;
    let fetcher = HttpFetcher::new()?;
    search(&roots, &registries, &fetcher, text, which)
}

/// `search [text]`: je Registry den Index holen und die Treffer in einer Tabelle zeigen —
/// je Name die höchste Version, installierte markiert. Eine nicht erreichbare Registry ist ein
/// Hinweis, alle zusammen ein Fehler.
pub(crate) fn search(
    roots: &Roots,
    registries: &[RegistryConfig],
    fetcher: &dyn Fetcher,
    text: Option<&str>,
    which: Option<&str>,
) -> Result<String> {
    let candidates = pick_registries(registries, which)?;
    let installed = Installed::load(roots)?;
    let mut rows: Vec<[String; 5]> = Vec::new();
    let mut failed = 0usize;
    for cfg in &candidates {
        match fetch_index(fetcher, cfg) {
            Ok(index) => {
                for e in index.matching(text) {
                    // Die Beschreibung kommt aus dem Index und wird von dessen `validate` nicht
                    // angefasst; hier an der Quelle bereinigt, damit auch ein Aufrufer geschützt
                    // ist, der `search` nicht über den gemeinsamen Ausgang druckt.
                    let mut desc = sanitize_display(e.description.as_deref().unwrap_or_default());
                    if let Some(i) = installed.packages.get(&e.name) {
                        if !desc.is_empty() {
                            desc.push_str(" · ");
                        }
                        desc.push_str(&format!("installiert {}", i.version));
                    }
                    rows.push([
                        e.name.clone(),
                        e.version.clone(),
                        e.publisher.clone(),
                        cfg.name.clone(),
                        desc,
                    ]);
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("Hinweis: nicht erreichbar — {}", plain(&cfg.name, &e));
            }
        }
    }
    if failed == candidates.len() {
        bail!("keine Registry erreichbar");
    }
    if rows.is_empty() {
        return Ok("keine Treffer\n".into());
    }
    rows.sort();
    let mut out = format!(
        "{:<20} {:<10} {:<14} {:<12} {}\n",
        "PAKET", "VERSION", "HERAUSGEBER", "REGISTRY", "BESCHREIBUNG"
    );
    for r in &rows {
        out.push_str(&format!(
            "{:<20} {:<10} {:<14} {:<12} {}\n",
            r[0], r[1], r[2], r[3], r[4]
        ));
    }
    out.push_str("\nInstallieren: `sepp pkg install <name>`\n");
    Ok(out)
}

fn untrust_cmd(name: &str) -> Result<String> {
    let roots = roots()?;
    let u = untrust_publisher(&roots, name)?;
    let mut text = format!(
        "Vertrauen in Herausgeber »{}« zurückgenommen (Fingerprint {}): {} gelöscht.\n",
        u.name,
        u.fingerprint,
        u.path.display()
    );
    if !u.installed.is_empty() {
        text.push_str(&format!(
            "Installierte Pakete bleiben: {} — beim nächsten Paket von »{}« wird der Fingerprint \
             neu bestätigt.\n",
            u.installed.join(", "),
            u.name
        ));
    }
    Ok(text)
}

/// Betreiber-Werkzeug: Index bauen, signieren, neben die Pakete schreiben — nie über vorhandene
/// Dateien hinweg.
fn index_cmd(
    dir: &Path,
    out: Option<&Path>,
    name: Option<&str>,
    base_url: Option<&str>,
    key: Option<&Path>,
) -> Result<String> {
    let key_path = match key {
        Some(k) => k.to_path_buf(),
        None => roots()?.registry_key_files().secret,
    };
    if !key_path.is_file() {
        bail!(
            "kein Betreiber-Schlüssel unter {} — erst `sepp pkg keygen --registry` (oder --key <pfad>)",
            key_path.display()
        );
    }
    let key = sepp_pkg::crypto::load_signing_key(&key_path)?;
    let name = match name {
        Some(n) => n.to_string(),
        None => {
            let base = dir
                .canonicalize()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default();
            validate_name(&base).map_err(|e| {
                anyhow!("{e} — der Verzeichnisname taugt nicht als Registry-Name; `--name <name>` angeben")
            })?;
            base
        }
    };
    let build = build_index(dir, &key, &name, base_url)?;
    let out_dir = out.unwrap_or(dir);
    std::fs::create_dir_all(out_dir).with_context(|| format!("{}", out_dir.display()))?;
    let index_path = out_dir.join(INDEX_FILE);
    let sig_path = out_dir.join(SIG_FILE);
    write_new(&index_path, build.index_text.as_bytes())?;
    if let Err(e) = write_new(&sig_path, build.signature_text.as_bytes()) {
        let _ = std::fs::remove_file(&index_path);
        return Err(e);
    }
    let mut text = format!(
        "Index geschrieben: {} + {} ({} Pakete: {})\nBetreiber-Fingerprint {}\n",
        index_path.display(),
        sig_path.display(),
        build.entries.len(),
        build
            .entries
            .iter()
            .map(|(n, v)| format!("{n} {v}"))
            .collect::<Vec<_>>()
            .join(", "),
        build.fingerprint
    );
    for w in &build.warnings {
        text.push_str(&format!("Hinweis: {w}\n"));
    }
    text.push_str(&format!(
        "\nBeide Dateien neben die Pakete legen. Nutzer tragen in ihre settings.toml ein:\n\n{}\n",
        registries_snippet(&name, &build.public_key)
    ));
    Ok(text)
}

/// Schreibt eine neue Datei; eine vorhandene wird nie überschrieben.
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "{}: wird nie überschrieben — zum Neubauen erst entfernen",
                path.display()
            )
        })?
        .write_all(bytes)
        .with_context(|| format!("{}", path.display()))
}

fn list_cmd() -> Result<String> {
    let roots = roots()?;
    let entries = list(&roots)?;
    let mut out = String::new();
    if entries.is_empty() {
        out.push_str(&format!(
            "Keine Pakete installiert ({}).\n",
            roots.pkg_dir().display()
        ));
    } else {
        out.push_str(&format!(
            "{:<20} {:<10} {:<14} {:<12} {:<16} {}\n",
            "PAKET", "VERSION", "HERAUSGEBER", "INSTALLIERT", "QUELLE", "INHALT"
        ));
        for e in entries {
            match e.receipt {
                Some(r) => {
                    let mut inhalt = Vec::new();
                    if !r.plugins.is_empty() {
                        inhalt.push(format!("Plugins {}", r.plugins.join(", ")));
                    }
                    if !r.hooks.is_empty() {
                        inhalt.push(format!("Hooks {}", r.hooks.join(", ")));
                    }
                    let dir_note = if e.dir_present {
                        ""
                    } else {
                        " (Verzeichnis fehlt!)"
                    };
                    out.push_str(&format!(
                        "{:<20} {:<10} {:<14} {:<12} {:<16} {}{dir_note}\n",
                        e.name,
                        r.version,
                        r.publisher,
                        date(r.installed_at),
                        r.source.as_deref().unwrap_or("-"),
                        inhalt.join(" · ")
                    ));
                }
                None => out.push_str(&format!(
                    "{:<20} {:<10} {:<14} {:<12} {:<16} ohne Nachweis — von Hand kopiert?\n",
                    e.name, "?", "?", "?", "-"
                )),
            }
        }
    }
    let trusted = trusted_publishers(&roots)?;
    if !trusted.is_empty() {
        out.push_str("\nVertraute Herausgeber:\n");
        for t in trusted {
            out.push_str(&format!(
                "  {:<14} {}  seit {}\n",
                t.name,
                t.fingerprint,
                date(t.trusted_at)
            ));
        }
    }
    Ok(out)
}

fn remove_cmd(name: &str) -> Result<String> {
    let roots = roots()?;
    let r = remove(&roots, name)?;
    let mut text = format!("{} entfernt:", r.name);
    if r.dir_removed {
        text.push_str(&format!(
            " Verzeichnis {}",
            roots.package_dir(name).display()
        ));
    }
    if r.policy_removed {
        text.push_str(&format!(
            "{} Rechte-Block in {}",
            if r.dir_removed { " ·" } else { "" },
            roots.policy_path().display()
        ));
    }
    if r.receipt_removed {
        text.push_str(" · Nachweis");
    }
    text.push('\n');
    if let Some(p) = r.publisher {
        text.push_str(&format!(
            "Das Vertrauen in Herausgeber »{p}« bleibt; zum Zurücknehmen `sepp pkg untrust {p}`.\n"
        ));
    }
    text.push_str("sepp neu starten, damit das Werkzeug verschwindet.\n");
    Ok(text)
}

/// `JJJJ-MM-TT` aus Unix-Sekunden — ohne Zeitbibliothek (Howard Hinnants `civil_from_days`).
fn date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_pkg_forms_and_errors() {
        assert_eq!(
            parse_pkg_args(&a(&["keygen"])).unwrap(),
            PkgCmd::Keygen { registry: false }
        );
        assert_eq!(
            parse_pkg_args(&a(&["keygen", "--registry"])).unwrap(),
            PkgCmd::Keygen { registry: true }
        );
        assert_eq!(
            parse_pkg_args(&a(&["untrust", "acme"])).unwrap(),
            PkgCmd::Untrust {
                name: "acme".into()
            }
        );
        assert_eq!(
            parse_pkg_args(&a(&[
                "index",
                "site",
                "--out",
                "out",
                "--name",
                "test",
                "--base-url",
                "https://x/y",
                "--key",
                "k"
            ]))
            .unwrap(),
            PkgCmd::Index {
                dir: "site".into(),
                out: Some("out".into()),
                name: Some("test".into()),
                base_url: Some("https://x/y".into()),
                key: Some("k".into())
            }
        );
        assert_eq!(parse_pkg_args(&a(&["list"])).unwrap(), PkgCmd::List);
        assert_eq!(
            parse_pkg_args(&a(&["remove", "demo"])).unwrap(),
            PkgCmd::Remove {
                name: "demo".into()
            }
        );
        assert_eq!(
            parse_pkg_args(&a(&["pack", "src", "--out", "x.seppkg", "--key", "k"])).unwrap(),
            PkgCmd::Pack {
                dir: "src".into(),
                out: Some("x.seppkg".into()),
                key: Some("k".into())
            }
        );
        assert_eq!(
            parse_pkg_args(&a(&[
                "install",
                "d.seppkg",
                "--yes",
                "--trust-key",
                "ab12",
                "--var",
                "A=1",
                "--var",
                "B=x=y"
            ]))
            .unwrap(),
            PkgCmd::Install {
                source: InstallSource::File("d.seppkg".into()),
                yes: true,
                trust_key: Some("ab12".into()),
                vars: vec![("A".into(), "1".into()), ("B".into(), "x=y".into())]
            }
        );
        assert_eq!(
            parse_pkg_args(&a(&["install", "demo"])).unwrap(),
            PkgCmd::Install {
                source: InstallSource::Registry {
                    spec: PkgSpec {
                        name: "demo".into(),
                        version: None
                    },
                    registry: None
                },
                yes: false,
                trust_key: None,
                vars: vec![]
            }
        );
        assert_eq!(
            parse_pkg_args(&a(&[
                "install",
                "demo@1.2.3",
                "--registry",
                "kionova",
                "-y"
            ]))
            .unwrap(),
            PkgCmd::Install {
                source: InstallSource::Registry {
                    spec: PkgSpec {
                        name: "demo".into(),
                        version: Some("1.2.3".into())
                    },
                    registry: Some("kionova".into())
                },
                yes: true,
                trust_key: None,
                vars: vec![]
            }
        );
        // Datei-Formen: Endung, Pfadtrenner, führender Punkt.
        for file in ["demo.seppkg", "./demo", "pfad/demo", "..\\demo"] {
            assert!(
                matches!(
                    parse_pkg_args(&a(&["install", file])).unwrap(),
                    PkgCmd::Install {
                        source: InstallSource::File(_),
                        ..
                    }
                ),
                "{file}"
            );
        }
        assert_eq!(
            parse_pkg_args(&a(&["search"])).unwrap(),
            PkgCmd::Search {
                text: None,
                registry: None
            }
        );
        assert_eq!(
            parse_pkg_args(&a(&["search", "rechnung", "--registry", "x"])).unwrap(),
            PkgCmd::Search {
                text: Some("rechnung".into()),
                registry: Some("x".into())
            }
        );
        for bad in [
            vec!["install", "x.seppkg", "--registry", "r"],
            vec!["install", "Demo"],
            vec!["install", "demo@eins"],
            vec!["install", "demo", "--registry", "Bad"],
            vec!["install", "demo", "--registry"],
            vec!["search", "a", "b"],
            vec!["search", "--bogus"],
            vec!["search", "--registry", "Bad"],
            vec!["untrust"],
            vec!["untrust", "a", "b"],
            vec!["keygen", "--bogus"],
            vec!["index"],
            vec!["index", "a", "--name"],
            vec!["index", "a", "--name", "Bad"],
            vec![],
            vec!["bogus"],
            vec!["keygen", "x"],
            vec!["remove"],
            vec!["remove", "a", "b"],
            vec!["pack"],
            vec!["pack", "a", "--out"],
            vec!["install"],
            vec!["install", "a", "--var", "ohne-gleich"],
            vec!["install", "a", "--var", "=wert"],
            vec!["install", "a", "--trust-key"],
            vec!["install", "a", "--bogus"],
        ] {
            assert!(parse_pkg_args(&a(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn date_formats_unix_seconds() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_788_000_000), "2026-08-29");
        assert_eq!(date(1_788_691_200), "2026-09-06");
    }

    /// Der ganze Weg mit einem echten Plugin: Beispiel bauen, Paket packen, gegen Wurzeln im
    /// Temp-Verzeichnis installieren, das Plugin aus `pkg/<name>/plugins` laden und aufrufen.
    /// `#[ignore]`, weil die CI kein wasm32-Target hat:
    ///
    /// ```bash
    /// cargo test -p sepp-cli -- --ignored package_with_textstat
    /// ```
    #[test]
    #[ignore = "braucht das Target wasm32-unknown-unknown"]
    fn package_with_textstat_installs_and_the_plugin_runs() {
        use sepp_tools::Tool as _;
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let example = repo.join("examples/textstat-plugin");
        let out = std::process::Command::new("cargo")
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-unknown-unknown",
                "--manifest-path",
            ])
            .arg(example.join("Cargo.toml"))
            .output()
            .expect("cargo startbar");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let wasm = example.join("target/wasm32-unknown-unknown/release/textstat.wasm");

        let tmp = tempfile::tempdir().unwrap();
        let roots = Roots {
            config: tmp.path().join("config"),
            state: tmp.path().join("state"),
        };
        std::fs::create_dir_all(&roots.config).unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("plugins")).unwrap();
        std::fs::create_dir_all(src.join("skills")).unwrap();
        std::fs::copy(&wasm, src.join("plugins/textstat.wasm")).unwrap();
        std::fs::copy(
            example.join("textstat.toml"),
            src.join("plugins/textstat.toml"),
        )
        .unwrap();
        std::fs::write(
            src.join("skills/zaehlen.md"),
            "Zähle Wörter mit textstat.\n",
        )
        .unwrap();
        std::fs::write(
            src.join("manifest.toml"),
            "format = 1\nname = \"demo\"\nversion = \"0.1.0\"\n[publisher]\nname = \"acme\"\n",
        )
        .unwrap();
        let fp = sepp_pkg::crypto::write_new_keypair(&roots.key_files()).unwrap();
        let key = sepp_pkg::crypto::load_signing_key(&roots.key_files().secret).unwrap();
        let pkg = tmp.path().join("demo-0.1.0.seppkg");
        let report = pack_dir(&src, &key, &pkg).unwrap();
        assert_eq!(report.fingerprint, fp);

        let archive = PkgArchive::open(&pkg).unwrap();
        let plan = plan_install(&roots, &archive).unwrap();
        trust_publisher(&roots, &plan.manifest().publisher, "test").unwrap();
        let ctx = ResolveCtx::from_env();
        let rights = resolve_rights(plan.manifest(), &BTreeMap::new(), &ctx).unwrap();
        assert!(check_rights(&plan, &rights, &ctx).unwrap().is_empty());
        apply_install(&roots, &archive, &plan, &BTreeMap::new(), &rights).unwrap();

        // Die Loader-Regel: pkg/<name>/plugins ist ein Plugin-Verzeichnis wie jedes andere.
        let dirs = sepp_pkg::package_dirs_in(&roots.pkg_dir());
        assert_eq!(dirs, vec![roots.package_dir("demo")]);
        let (plugins, notes) = sepp_wasm::WasmHost::new()
            .discover_with(&roots.package_dir("demo").join("plugins"), &|_| None);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].spec().name, "textstat");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt
            .block_on(plugins[0].execute(
                serde_json::json!({ "text": "eins zwei drei" }),
                tokio_util::sync::CancellationToken::new(),
                None,
            ))
            .unwrap();
        assert_eq!(res.details["words"], 3);

        let r = remove(&roots, "demo").unwrap();
        assert!(r.dir_removed && r.receipt_removed && !r.policy_removed);
        assert!(sepp_pkg::package_dirs_in(&roots.pkg_dir()).is_empty());
    }

    #[test]
    fn name_and_version_come_from_the_manifest() {
        let (n, v) = name_and_version("name = \"demo\"\nversion = \"1.2.3\"\n").unwrap();
        assert_eq!((n.as_str(), v.as_str()), ("demo", "1.2.3"));
        assert!(name_and_version("name = \"demo\"\n").is_err());
    }

    /// Quelle für ein Testpaket: Skill und Fake-Plugin, Rechte nur `net` (keine Pfade), keine
    /// offene Variable — unter `cargo test` aus einem Terminal ist `interactive()` wahr, eine
    /// Frage hinge an stdin.
    fn demo_source(src: &Path) {
        std::fs::create_dir_all(src.join("skills")).unwrap();
        std::fs::create_dir_all(src.join("plugins")).unwrap();
        std::fs::write(src.join("skills/zaehlen.md"), "Zähle Wörter.\n").unwrap();
        std::fs::write(src.join("plugins/zaehler.wasm"), b"\0asm\x01\0\0\0fake").unwrap();
        std::fs::write(
            src.join("plugins/zaehler.toml"),
            "name = \"zaehler\"\nabi = 1\n[capabilities]\nnet = [\"api.example.com\"]\n",
        )
        .unwrap();
        std::fs::write(
            src.join("manifest.toml"),
            "format = 1\nname = \"demo\"\nversion = \"0.1.0\"\ndescription = \"Demo-Paket\"\n\n\
             [publisher]\nname = \"acme\"\n\n[rights.zaehler]\nnet = [\"api.example.com\"]\n",
        )
        .unwrap();
    }

    /// `sepp pkg index` gegen ein gepacktes Paket: beide Dateien entstehen, der Index verifiziert
    /// mit dem gezeigten Schlüssel, ein zweiter Lauf überschreibt nichts.
    #[test]
    fn index_cmd_writes_signed_files_and_never_overwrites() {
        use sepp_pkg::{verify_index, KeyFiles, SigningKey};

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let src = base.join("src");
        demo_source(&src);
        let (pub_key, _) = SigningKey::generate().unwrap();
        let site = base.join("site");
        std::fs::create_dir_all(&site).unwrap();
        pack_dir(&src, &pub_key, &site.join("demo-0.1.0.seppkg")).unwrap();
        let files = KeyFiles::registry_in_dir(&base.join("keys"));
        sepp_pkg::crypto::write_new_keypair(&files).unwrap();
        let public = std::fs::read_to_string(&files.public).unwrap();

        // Ohne --name: der Verzeichnisname `site` ist ein gültiger Registry-Name.
        let text = index_cmd(&site, None, None, None, Some(&files.secret)).unwrap();
        assert!(text.contains("1 Pakete: demo 0.1.0"), "{text}");
        assert!(
            text.contains(&format!("key = \"{}\"", public.trim())),
            "{text}"
        );
        let index = std::fs::read(site.join("index.toml")).unwrap();
        let sig = std::fs::read_to_string(site.join("index.sig")).unwrap();
        let parsed = verify_index(&index, &sig, public.trim()).unwrap();
        assert_eq!(parsed.name, "site");
        assert_eq!(parsed.packages[0].url, "demo-0.1.0.seppkg");

        let e = index_cmd(&site, None, Some("test"), None, Some(&files.secret))
            .unwrap_err()
            .to_string();
        assert!(e.contains("nie überschrieben"), "{e}");
        // Fehlender Schlüssel → Hinweis auf keygen --registry.
        let e = index_cmd(
            &site,
            Some(&base.join("out")),
            None,
            None,
            Some(&base.join("fehlt.key")),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("keygen --registry"), "{e}");
        // --out in ein frisches Verzeichnis, mit --base-url absolut.
        let text = index_cmd(
            &site,
            Some(&base.join("out")),
            Some("test"),
            Some("https://pkg.example/r"),
            Some(&files.secret),
        )
        .unwrap();
        assert!(text.contains("name = \"test\""), "{text}");
        let index = std::fs::read_to_string(base.join("out").join("index.toml")).unwrap();
        assert!(
            index.contains("url = \"https://pkg.example/r/demo-0.1.0.seppkg\""),
            "{index}"
        );
    }

    /// Statischer Datei-Server auf 127.0.0.1: liefert Dateien aus `dir` (Pfad aus der
    /// Request-Zeile), sonst 404; `Connection: close`; bedient Verbindungen bis zum Prozessende.
    fn serve_dir(dir: PathBuf) -> String {
        use std::io::Read as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { break };
                sock.set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while sock.read(&mut b).map(|n| n == 1).unwrap_or(false) {
                    head.push(b[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .trim_start_matches('/')
                    .to_string();
                let body = if path.contains("..") {
                    None
                } else {
                    std::fs::read(dir.join(&path)).ok()
                };
                let resp = match &body {
                    Some(b) => format!(
                        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                        b.len()
                    ),
                    None => {
                        "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                            .to_string()
                    }
                };
                let _ = sock.write_all(resp.as_bytes());
                if let Some(b) = body {
                    let _ = sock.write_all(&b);
                }
                let _ = sock.flush();
            }
        });
        format!("http://{addr}")
    }

    /// Der ganze Registry-Weg gegen einen lokalen Server, ohne wasm32 und ohne Umgebung: Paket
    /// packen, Index bauen und signieren, ausliefern, `install demo` mit vorweggenommenen
    /// Antworten, danach `search`; dazu die Negativfälle (nicht im Index, fremde Registry, keine
    /// Registry, manipuliertes Paket, fremder Index-Schlüssel).
    #[test]
    fn registry_install_end_to_end_on_loopback() {
        use sepp_pkg::crypto::encode_pubkey;
        use sepp_pkg::{build_index, SigningKey};

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let src = base.join("src");
        demo_source(&src);
        std::fs::create_dir_all(src.join("plugins")).unwrap();
        let (pub_key, _) = SigningKey::generate().unwrap();
        let site = base.join("site");
        std::fs::create_dir_all(&site).unwrap();
        let pkg_path = site.join("demo-0.1.0.seppkg");
        pack_dir(&src, &pub_key, &pkg_path).unwrap();

        let (reg_key, _) = SigningKey::generate().unwrap();
        let build = build_index(&site, &reg_key, "test", None).unwrap();
        std::fs::write(site.join("index.toml"), &build.index_text).unwrap();
        std::fs::write(site.join("index.sig"), &build.signature_text).unwrap();
        let url = serve_dir(site.clone());

        let roots = Roots {
            config: base.join("config"),
            state: base.join("state"),
        };
        std::fs::create_dir_all(&roots.config).unwrap();
        let registries = vec![RegistryConfig {
            name: "test".into(),
            url: format!("{url}/index.toml"),
            key: build.public_key.clone(),
        }];
        let fetcher = HttpFetcher::new().unwrap();
        let fp = pub_key.fingerprint();
        let answers = Answers {
            yes: true,
            trust_key: Some(&fp),
            vars: &[],
        };
        let spec = parse_spec("demo").unwrap();

        // Negativ, bevor etwas installiert ist.
        let e = install_from_registry(
            &roots,
            &registries,
            &fetcher,
            &parse_spec("demo@9.9.9").unwrap(),
            None,
            &answers,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("9.9.9") && e.contains("0.1.0"), "{e}");
        let e = install_from_registry(
            &roots,
            &registries,
            &fetcher,
            &spec,
            Some("fremd"),
            &answers,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("fremd"), "{e}");
        let e = install_from_registry(&roots, &[], &fetcher, &spec, None, &answers)
            .unwrap_err()
            .to_string();
        assert!(e.contains("keine Registry"), "{e}");
        let other = SigningKey::generate().unwrap().0;
        let wrong_key = vec![RegistryConfig {
            key: encode_pubkey(&other.public_key()),
            ..registries[0].clone()
        }];
        let e = install_from_registry(&roots, &wrong_key, &fetcher, &spec, None, &answers)
            .unwrap_err()
            .to_string();
        assert!(e.contains("Signatur"), "{e}");
        assert!(!roots.package_dir("demo").exists());

        // Erfolg.
        let text =
            install_from_registry(&roots, &registries, &fetcher, &spec, None, &answers).unwrap();
        assert!(text.contains("demo 0.1.0 installiert"), "{text}");
        assert!(roots
            .package_dir("demo")
            .join("plugins/zaehler.wasm")
            .is_file());
        let receipt = Installed::load(&roots).unwrap();
        assert_eq!(
            receipt.packages["demo"].source.as_deref(),
            Some("registry:test")
        );
        let downloads = roots.downloads_dir();
        assert!(
            std::fs::read_dir(&downloads).unwrap().next().is_none(),
            "downloads/ leer"
        );
        let policy = std::fs::read_to_string(roots.policy_path()).unwrap();
        assert!(policy.contains("# von sepp pkg: demo 0.1.0"), "{policy}");
        let trusted = std::fs::read_to_string(roots.trusted_keys_dir().join("acme.json")).unwrap();
        assert!(trusted.contains("aus Registry test"), "{trusted}");

        // search: Treffer mit Markierung, Filter ohne Treffer, fremde Registry.
        let s = search(&roots, &registries, &fetcher, None, None).unwrap();
        assert!(
            s.contains("demo") && s.contains("installiert 0.1.0") && s.contains("Demo-Paket"),
            "{s}"
        );
        assert_eq!(
            search(&roots, &registries, &fetcher, Some("nix"), None).unwrap(),
            "keine Treffer\n"
        );
        assert!(search(&roots, &registries, &fetcher, None, Some("fremd")).is_err());
        assert!(search(&roots, &wrong_key, &fetcher, None, None).is_err());

        // Manipuliertes Paket nach dem Indexbau → SHA-256, nichts auf Platte.
        let original = std::fs::read(&pkg_path).unwrap();
        let mut bytes = original.clone();
        let i = bytes.len() / 2;
        bytes[i] ^= 0x55;
        std::fs::write(&pkg_path, &bytes).unwrap();
        let e = install_from_registry(&roots, &registries, &fetcher, &spec, None, &answers)
            .unwrap_err()
            .to_string();
        assert!(e.contains("SHA-256"), "{e}");
        assert!(std::fs::read_dir(&downloads).unwrap().next().is_none());
        std::fs::write(&pkg_path, &original).unwrap();

        // Gleiche Version noch einmal → der normale Weg meldet „nicht neuer".
        let e = install_from_registry(&roots, &registries, &fetcher, &spec, None, &answers)
            .unwrap_err()
            .to_string();
        assert!(e.contains("nicht neuer"), "{e}");
        assert!(std::fs::read_dir(&downloads).unwrap().next().is_none());
    }
}
