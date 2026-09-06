//! `sepp pkg keygen | pack | install | list | remove` — Pakete bauen und installieren.
//!
//! Die Logik liegt in `sepp-pkg` und arbeitet gegen abstrakte Wurzeln; hier stehen Parsing,
//! Pfade aus `session`, und die Dialoge. Alles, was der Nutzer beantworten muss (Vertrauen in
//! einen neuen Herausgeber, Variablen, die Zustimmung zu den Rechten), läuft über **stderr** und
//! stdin — stdout bleibt der Datenkanal und bekommt nur die Abschlussmeldung. Nicht-interaktiv
//! (Pipe, Skript) gibt es keine Fragen: Dann müssen `--yes`, `--trust-key` und `--var` alles
//! vorwegnehmen, sonst bricht `install` mit der Liste ab, der man zugestimmt hätte.
//!
//! Wie die anderen Unterbefehle: `parse_*` ist rein, `run_*` liefert den `ExitCode`, innen
//! `anyhow` und eine zurückgegebene Meldung. Kein Tokio.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};

use sepp_pkg::{
    apply_install, check_collisions, check_rights, consent_lines, list, pack_dir, plan_install,
    remove, resolve_rights, resolve_vars, trust_publisher, trusted_publishers, value_notes,
    PkgArchive, Roots, TrustStatus, EXTENSION,
};
use sepp_policy::ResolveCtx;

use crate::session;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkgCmd {
    /// Schlüsselpaar unter `<state_root>/pkg/` anlegen.
    Keygen,
    /// Verzeichnis zu `<name>-<version>.seppkg` packen und signieren.
    Pack {
        dir: PathBuf,
        out: Option<PathBuf>,
        key: Option<PathBuf>,
    },
    Install {
        file: PathBuf,
        yes: bool,
        trust_key: Option<String>,
        vars: Vec<(String, String)>,
    },
    List,
    Remove {
        name: String,
    },
}

const USAGE: &str = "Verwendung: sepp pkg keygen\n\
                     \x20           sepp pkg pack <dir> [--out <datei>] [--key <pfad>]\n\
                     \x20           sepp pkg install <datei.seppkg> [--yes] [--trust-key <fingerprint>] [--var NAME=WERT]…\n\
                     \x20           sepp pkg list\n\
                     \x20           sepp pkg remove <name>";

/// Zerlegt die Argumente hinter `sepp pkg`. Reine Funktion.
pub fn parse_pkg_args(args: &[String]) -> std::result::Result<PkgCmd, String> {
    let Some(sub) = args.first().map(String::as_str) else {
        return Err(format!("pkg: Unterbefehl fehlt\n{USAGE}"));
    };
    let rest = &args[1..];
    match sub {
        "keygen" => {
            reject_extra(rest, "keygen")?;
            Ok(PkgCmd::Keygen)
        }
        "list" => {
            reject_extra(rest, "list")?;
            Ok(PkgCmd::List)
        }
        "remove" => {
            let mut name = None;
            for a in rest {
                if a.starts_with('-') {
                    return Err(format!("pkg remove: unbekannte Option '{a}'\n{USAGE}"));
                }
                if name.is_some() {
                    return Err(format!("pkg remove: unerwartetes Argument '{a}'\n{USAGE}"));
                }
                name = Some(a.clone());
            }
            let name = name.ok_or_else(|| format!("pkg remove: Name fehlt\n{USAGE}"))?;
            Ok(PkgCmd::Remove { name })
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
                        return Err(format!("pkg pack: unerwartetes Argument '{other}'\n{USAGE}"))
                    }
                }
            }
            let dir = dir.ok_or_else(|| format!("pkg pack: Verzeichnis fehlt\n{USAGE}"))?;
            Ok(PkgCmd::Pack { dir, out, key })
        }
        "install" => {
            let mut file = None;
            let mut yes = false;
            let mut trust_key = None;
            let mut vars = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--yes" | "-y" => yes = true,
                    "--trust-key" => trust_key = Some(value(&mut it, "--trust-key")?),
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
                        return Err(format!(
                            "pkg install: unbekannte Option '{other}'\n{USAGE}"
                        ))
                    }
                    other if file.is_none() => file = Some(PathBuf::from(other)),
                    other => {
                        return Err(format!(
                            "pkg install: unerwartetes Argument '{other}'\n{USAGE}"
                        ))
                    }
                }
            }
            let file = file.ok_or_else(|| format!("pkg install: Datei fehlt\n{USAGE}"))?;
            Ok(PkgCmd::Install {
                file,
                yes,
                trust_key,
                vars,
            })
        }
        other => Err(format!(
            "pkg: unbekannter Unterbefehl '{other}' (erlaubt: keygen, pack, install, list, remove)\n{USAGE}"
        )),
    }
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
        PkgCmd::Keygen => keygen(),
        PkgCmd::Pack { dir, out, key } => pack(&dir, out.as_deref(), key.as_deref()),
        PkgCmd::Install {
            file,
            yes,
            trust_key,
            vars,
        } => install(&file, yes, trust_key.as_deref(), &vars),
        PkgCmd::List => list_cmd(),
        PkgCmd::Remove { name } => remove_cmd(&name),
    };
    match result {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Fehler: {e}");
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

/// Fragen gibt es nur, wenn jemand antworten kann.
fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

fn ask(prompt: &str, default: Option<&str>) -> Result<String> {
    let mut err = std::io::stderr();
    match default {
        Some(d) => write!(err, "{prompt} [{d}]: ")?,
        None => write!(err, "{prompt}: ")?,
    }
    err.flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim();
    if line.is_empty() {
        return Ok(default.unwrap_or("").to_string());
    }
    Ok(line.to_string())
}

fn confirm(question: &str) -> Result<bool> {
    let a = ask(&format!("{question} [j/N]"), None)?;
    Ok(matches!(
        a.to_lowercase().as_str(),
        "j" | "ja" | "y" | "yes"
    ))
}

fn keygen() -> Result<String> {
    let roots = roots()?;
    let files = roots.key_files();
    let fp = sepp_pkg::crypto::write_new_keypair(&files)?;
    Ok(format!(
        "Schlüsselpaar angelegt:\n  {}  (geheim, 0600 — nie weitergeben)\n  {}\n\
         Fingerprint {fp}\n\n\
         Damit signiert `sepp pkg pack`. Nutzer sehen den Fingerprint beim ersten Paket und \
         bestätigen ihn einmal.\n",
        files.secret.display(),
        files.public.display()
    ))
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

fn install(
    file: &Path,
    yes: bool,
    trust_key: Option<&str>,
    given: &[(String, String)],
) -> Result<String> {
    let roots = roots()?;
    let ctx = ResolveCtx::from_env();
    let archive = PkgArchive::open(file)?;
    let plan = plan_install(&roots, &archive)?;
    let manifest = plan.manifest();
    let unknown = manifest.unknown_keys();
    if !unknown.is_empty() {
        eprintln!(
            "Hinweis: unbekannte Felder im Paket-Manifest, ohne Wirkung: {}",
            unknown.join(", ")
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
             wirklich gewechselt hat: {} löschen und neu installieren.",
            roots.trusted_keys_dir().join(format!("{name}.json")).display()
        ),
        TrustStatus::New { name, fingerprint } => {
            let accepted = match trust_key {
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
            trust_publisher(
                &roots,
                &manifest.publisher,
                &format!("install {}", file.display()),
            )?;
        }
    }

    // 2 · Variablen.
    let given_map: BTreeMap<String, String> = given.iter().cloned().collect();
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
    let collisions = check_collisions(&roots, &plan);
    if !collisions.errors.is_empty() {
        bail!(
            "Installation nicht möglich:\n  {}",
            collisions.errors.join("\n  ")
        );
    }
    warnings.extend(collisions.warnings);

    // 4 · Zustimmung.
    let lines = consent_lines(&plan, &rights, &values, &warnings);
    eprintln!("{}", lines.join("\n"));
    let approved = if yes {
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
    let report = apply_install(&roots, &archive, &plan, &values, &rights)?;
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
            "{:<20} {:<10} {:<14} {:<12} {}\n",
            "PAKET", "VERSION", "HERAUSGEBER", "INSTALLIERT", "INHALT"
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
                        "{:<20} {:<10} {:<14} {:<12} {}{dir_note}\n",
                        e.name,
                        r.version,
                        r.publisher,
                        date(r.installed_at),
                        inhalt.join(" · ")
                    ));
                }
                None => out.push_str(&format!(
                    "{:<20} {:<10} {:<14} {:<12} ohne Nachweis — von Hand kopiert?\n",
                    e.name, "?", "?", "?"
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
            "Das Vertrauen in Herausgeber »{p}« bleibt ({}); zum Zurücknehmen die Datei löschen.\n",
            roots.trusted_keys_dir().join(format!("{p}.json")).display()
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
        assert_eq!(parse_pkg_args(&a(&["keygen"])).unwrap(), PkgCmd::Keygen);
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
                file: "d.seppkg".into(),
                yes: true,
                trust_key: Some("ab12".into()),
                vars: vec![("A".into(), "1".into()), ("B".into(), "x=y".into())]
            }
        );
        for bad in [
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
}
