//! `sepp plugin new <name> [--dir <pfad>] [--sdk-path <pfad>]` — das Gerüst für ein WASM-Plugin
//! (Tier 2) mit dem SDK `sepp-plugin`.
//!
//! Legt ein Cargo-Crate an, das nach dem Bauen für `wasm32-unknown-unknown` direkt in
//! `~/.sepp/plugins/` kopiert werden kann: `Cargo.toml`, `src/lib.rs` (ein Werkzeug im
//! Zielbild des SDK samt nativem Test), `<name>.toml` (Manifest), `README.md`, `.gitignore`.
//!
//! **Der Name ist zugleich** Paket-, Bibliotheks-, Funktions-, Werkzeug- und Manifestname
//! (`<name>.wasm` neben `<name>.toml`). Deshalb ist die Regel bewusst enger als die des Hosts
//! (`^[a-z][a-z0-9_]{0,63}$`, kein Rust-Schlüsselwort): So braucht es keine Umschreibung
//! zwischen Bindestrich und Unterstrich, und der Loader findet das Manifest ohne Zutun.
//!
//! Wie `sepp init`: Vorhandene Dateien werden **nie** überschrieben, jede Datei wird als
//! `angelegt:` oder `übersprungen (existiert):` gemeldet, `anyhow` innen, `ExitCode` außen,
//! kein Tokio.
//!
//! **Die SDK-Dependency:** Die sepp-Crates liegen nicht auf crates.io. Das Gerüst zeigt deshalb
//! per Git-Tag auf die Version des laufenden `sepp` (CLI-Version = SDK-Version = Tag). Für die
//! Entwicklung im Repo oder gegen einen lokalen Checkout schreibt `--sdk-path` stattdessen
//! eine `path`-Dependency.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};

/// Wo das SDK herkommt, wenn kein `--sdk-path` angegeben ist.
const SDK_GIT: &str = "https://github.com/Vezir0013/sepp-mini";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginCmd {
    /// `sepp plugin new <name>` — Gerüst in `<dir>` (Default `./<name>`).
    New {
        name: String,
        dir: Option<PathBuf>,
        sdk_path: Option<PathBuf>,
    },
}

const USAGE: &str = "Verwendung: sepp plugin new <name> [--dir <pfad>] [--sdk-path <pfad>]";

/// Zerlegt die Argumente hinter `sepp plugin`. Reine Funktion (testbar ohne Dateisystem).
pub fn parse_plugin_args(args: &[String]) -> Result<PluginCmd, String> {
    match args.first().map(String::as_str) {
        Some("new") => {}
        Some(other) => {
            return Err(format!(
                "plugin: unbekannter Unterbefehl '{other}' (erlaubt: new)\n{USAGE}"
            ))
        }
        None => return Err(format!("plugin: Unterbefehl fehlt (erlaubt: new)\n{USAGE}")),
    }
    let mut name: Option<String> = None;
    let mut dir: Option<PathBuf> = None;
    let mut sdk_path: Option<PathBuf> = None;
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dir" => {
                let v = it
                    .next()
                    .ok_or_else(|| format!("plugin new: --dir braucht einen Pfad\n{USAGE}"))?;
                dir = Some(PathBuf::from(v));
            }
            "--sdk-path" => {
                let v = it
                    .next()
                    .ok_or_else(|| format!("plugin new: --sdk-path braucht einen Pfad\n{USAGE}"))?;
                sdk_path = Some(PathBuf::from(v));
            }
            other if other.starts_with('-') => {
                return Err(format!("plugin new: unbekannte Option '{other}'\n{USAGE}"))
            }
            other if name.is_none() => name = Some(other.to_string()),
            other => {
                return Err(format!(
                    "plugin new: unerwartetes Argument '{other}'\n{USAGE}"
                ))
            }
        }
    }
    let name = name.ok_or_else(|| format!("plugin new: Name fehlt\n{USAGE}"))?;
    Ok(PluginCmd::New {
        name,
        dir,
        sdk_path,
    })
}

/// Rust-Schlüsselwörter (strikt und reserviert, alle kleingeschrieben) sowie Crate-Namen, die
/// als `[lib] name` oder Funktionsname kollidieren würden.
const RESERVED: &[&str] = &[
    "abstract",
    "alloc",
    "as",
    "async",
    "await",
    "become",
    "box",
    "break",
    "const",
    "continue",
    "core",
    "crate",
    "do",
    "dyn",
    "else",
    "enum",
    "extern",
    "false",
    "final",
    "fn",
    "for",
    "gen",
    "if",
    "impl",
    "in",
    "let",
    "loop",
    "macro",
    "match",
    "mod",
    "move",
    "mut",
    "override",
    "priv",
    "proc_macro",
    "pub",
    "ref",
    "return",
    "schemars",
    "self",
    "sepp_plugin",
    "serde",
    "static",
    "std",
    "struct",
    "super",
    "test",
    "trait",
    "true",
    "try",
    "type",
    "typeof",
    "unsafe",
    "unsized",
    "use",
    "virtual",
    "where",
    "while",
    "yield",
];

/// Prüft den Plugin-Namen: `^[a-z][a-z0-9_]{0,63}$`, kein Schlüsselwort — und zur Sicherheit
/// zusätzlich die Regel des Hosts, damit beide nie auseinanderlaufen.
pub fn validate_name(name: &str) -> Result<(), String> {
    let rule = "erlaubt sind 1 bis 64 Zeichen aus a-z, 0-9 und _, beginnend mit einem \
                Buchstaben (der Name ist zugleich Paket-, Datei- und Werkzeugname)";
    let mut chars = name.chars();
    let ok_first = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    let ok_rest = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ok_first || !ok_rest || name.len() > 64 {
        return Err(format!("Plugin-Name {name:?} ist unzulässig — {rule}"));
    }
    if RESERVED.contains(&name) {
        return Err(format!(
            "Plugin-Name {name:?} ist ein Schlüsselwort oder reservierter Crate-Name — anders nennen"
        ));
    }
    if !sepp_core::is_valid_tool_name(name) {
        return Err(format!(
            "Plugin-Name {name:?} ist als Werkzeugname unzulässig — {rule}"
        ));
    }
    Ok(())
}

pub fn run_plugin(cmd: PluginCmd) -> ExitCode {
    let PluginCmd::New {
        name,
        dir,
        sdk_path,
    } = cmd;
    match new_plugin(&name, dir, sdk_path) {
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

/// Legt das Gerüst an und liefert die Abschlussmeldung (nächste Schritte).
fn new_plugin(name: &str, dir: Option<PathBuf>, sdk_path: Option<PathBuf>) -> Result<String> {
    validate_name(name).map_err(|e| anyhow!("{e}"))?;
    let dir = dir.unwrap_or_else(|| PathBuf::from(name));
    let sdk_dep = match sdk_path {
        Some(p) => {
            let p = p
                .canonicalize()
                .with_context(|| format!("--sdk-path {}: nicht gefunden", p.display()))?;
            if !p.join("Cargo.toml").is_file() {
                return Err(anyhow!(
                    "--sdk-path {}: dort liegt keine Cargo.toml (erwartet: <repo>/crates/sepp-plugin)",
                    p.display()
                ));
            }
            sdk_dep_path(&p)
        }
        None => sdk_dep_git(env!("CARGO_PKG_VERSION")),
    };
    scaffold(&dir, name, &sdk_dep)?;
    Ok(format!(
        "\nPlugin-Gerüst `{name}` liegt in {dir}.\n\n\
         Nächste Schritte:\n\
         \x20 cd {dir}\n\
         \x20 cargo test                                            # nativ, ohne wasm32-Target\n\
         \x20 cargo build --release --target wasm32-unknown-unknown  # rustup target add wasm32-unknown-unknown\n\
         \x20 cp target/wasm32-unknown-unknown/release/{name}.wasm ~/.sepp/plugins/\n\
         \x20 cp {name}.toml ~/.sepp/plugins/\n\n\
         Rechte (falls das Manifest welche anfordert): sepp policy allow --global plugin.{name} <recht> <wert>\n",
        dir = dir.display()
    ))
}

/// Die SDK-Dependency als Git-Tag der laufenden Version — die einzige Form, die ohne Checkout
/// funktioniert, solange die Crates nicht auf crates.io liegen.
fn sdk_dep_git(version: &str) -> String {
    format!("git = \"{SDK_GIT}\", tag = \"v{version}\"")
}

fn sdk_dep_path(path: &Path) -> String {
    // Cargo will `/` auch unter Windows; ein Backslash in einem TOML-Basic-String wäre ein Escape.
    let p = path.display().to_string().replace('\\', "/");
    format!("path = \"{p}\"")
}

/// Ersetzt die Platzhalter `{{name}}` und `{{sdk_dep}}`. Bewusst `str::replace` statt `format!`:
/// Die Vorlagen sind voller geschweifter Klammern.
fn render(template: &str, name: &str, sdk_dep: &str) -> String {
    template
        .replace("{{name}}", name)
        .replace("{{sdk_dep}}", sdk_dep)
}

/// Schreibt alle Dateien des Gerüsts; vorhandene bleiben unangetastet.
fn scaffold(dir: &Path, name: &str, sdk_dep: &str) -> Result<()> {
    ensure_dir(dir)?;
    ensure_dir(&dir.join("src"))?;
    let files: [(PathBuf, String); 5] = [
        (
            dir.join("Cargo.toml"),
            render(CARGO_TEMPLATE, name, sdk_dep),
        ),
        (dir.join("src/lib.rs"), render(LIB_TEMPLATE, name, sdk_dep)),
        (
            dir.join(format!("{name}.toml")),
            render(MANIFEST_TEMPLATE, name, sdk_dep),
        ),
        (
            dir.join("README.md"),
            render(README_TEMPLATE, name, sdk_dep),
        ),
        (dir.join(".gitignore"), "/target\n".to_string()),
    ];
    for (path, content) in files {
        write_new(&path, &content)?;
    }
    Ok(())
}

fn ensure_dir(p: &Path) -> Result<()> {
    if p.is_dir() {
        println!("übersprungen (existiert): {}", p.display());
    } else {
        std::fs::create_dir_all(p).with_context(|| format!("anlegen: {}", p.display()))?;
        println!("angelegt: {}", p.display());
    }
    Ok(())
}

fn write_new(path: &Path, content: &str) -> Result<()> {
    if path.exists() {
        println!("übersprungen (existiert): {}", path.display());
    } else {
        std::fs::write(path, content).with_context(|| format!("schreiben: {}", path.display()))?;
        println!("angelegt: {}", path.display());
    }
    Ok(())
}

const CARGO_TEMPLATE: &str = r#"# {{name}} — ein WASM-Plugin für sepp mini (Tier 2). Erzeugt von `sepp plugin new`.
[package]
name = "{{name}}"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
# Der Loader sucht das Manifest als `<stamm>.toml` neben `<stamm>.wasm`; dieser Name ist der Stamm.
name = "{{name}}"
crate-type = ["cdylib"]

[dependencies]
# Das SDK kapselt Exports, Zeiger und Abholweg. Features `fs-read` / `net` NUR setzen, wenn das
# Manifest ({{name}}.toml) das Recht anfordert — sie schalten den zugehörigen Host-Import frei,
# und ein Modul, das eine Funktion ohne Gewährung importiert, lädt nicht.
# Lokaler Checkout statt Git: sepp-plugin = { path = "/pfad/zu/sepp-mini/crates/sepp-plugin" }
sepp-plugin = { {{sdk_dep}} }
# serde und schemars müssen direkte Deps sein: Ihre Derive-Makros verlangen das.
serde = { version = "1", features = ["derive"] }
schemars = "1"

[profile.release]
opt-level = "z"      # auf Größe optimieren — das Modul wird bei jedem Tool-Aufruf instanziiert
lto = true
codegen-units = 1
panic = "abort"      # kein Unwinding in WASM; spart Code und vermeidet halbe Zustände
strip = true
"#;

const LIB_TEMPLATE: &str = r##"//! `{{name}}` — ein WASM-Plugin für sepp mini, geschrieben mit dem SDK `sepp-plugin`.
//!
//! Bauen, installieren, Rechte: siehe `README.md` daneben. Das Aufrufprotokoll des Hosts
//! übernimmt `#[sepp_plugin::tool]`; hier steht nur die Arbeit.

use sepp_plugin::prelude::*;

/// Die Parameter des Werkzeugs. Das JSON-Schema fürs Modell entsteht hieraus; Doc-Kommentare
/// an den Feldern werden zu Beschreibungen.
#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Der Text, mit dem das Werkzeug arbeitet.
    text: String,
}

#[sepp_plugin::tool(desc = "Beschreibt, was das Werkzeug tut — das liest das Modell.")]
fn {{name}}(args: Args, host: &Host) -> Result<ToolResult> {
    host.log(&format!("{{name}}: {} Bytes erhalten", args.text.len()));
    // Hier die Arbeit. Fehler per `?` oder `Err("…".into())` — das SDK macht daraus ein
    // Ergebnis mit `is_error = true`; ein Plugin trappt nie. Dateien: `host.fs()` (Feature
    // `fs-read`), Netz: `host.http()` (Feature `net`) — jeweils nur mit Recht im Manifest.
    let words = args.text.split_whitespace().count();
    Ok(ToolResult::text(format!("{words} Wörter")).with_details(json!({ "words": words })))
}

#[cfg(test)]
mod tests {
    //! Läuft nativ, ohne wasm32-Target: `cargo test`.
    use super::*;
    use sepp_plugin::serde_json;

    #[test]
    fn spec_and_call() {
        let spec: ToolSpec = serde_json::from_str(&__sepp_plugin_export::spec_json()).unwrap();
        assert_eq!(spec.name, "{{name}}");

        let out = __sepp_plugin_export::call_json(br#"{"text":"a b"}"#);
        let r: ToolResult = serde_json::from_str(&out).unwrap();
        assert!(!r.is_error, "{r:?}");
        assert_eq!(r.details["words"], 2);
    }
}
"##;

const MANIFEST_TEMPLATE: &str = r#"# Manifest für das Plugin `{{name}}` — liegt neben `{{name}}.wasm` in ~/.sepp/plugins/.
# Es ist die Selbstauskunft des Autors; wirksam wird ein Recht erst durch die Gegenzeichnung
# in der policy.toml des Nutzers (`sepp policy allow --global plugin.{{name}} <recht> <wert>`).

name = "{{name}}"
# Version des Plugin-Protokolls, gegen die gebaut wurde (sepp lehnt höhere Werte ab).
abi = 1
version     = "0.1.0"
kind        = "wasm"
entry       = "{{name}}.wasm"
description = "Beschreibt, was das Werkzeug tut."

# Rechte, die das Plugin braucht. Jeder Eintrag braucht das passende Cargo-Feature am SDK
# (fs_read → "fs-read", net → "net"), sonst importiert das Modul die Funktion nicht und das
# Recht bleibt wirkungslos. Umgekehrt lädt ein Modul mit Feature, aber ohne Gewährung, nicht.
# `env` nennt die Variablen, die als `$NAME` in HTTP-Header-Werten eingesetzt werden dürfen —
# der Host ersetzt sie, das Modul sieht den Wert nie; nur zusammen mit `net`.
# [capabilities]
# fs_read = ["./daten"]
# net = ["api.example.com"]
# env = ["EXAMPLE_TOKEN"]

[limits]
max_memory_pages = 64        # à 64 KiB → 4 MiB
# wasmi ist ein Interpreter, grob 10- bis 20-mal langsamer als nativ. Wer Dokumente verarbeitet,
# testet an einer zweiseitigen Rechnung, setzt 5000 — und der erste 90-seitige Sammelbeleg
# bricht ab. Großzügig wählen; 0 = unbegrenzt (bleibt per Ctrl+C abbrechbar). Die Uhr läuft
# auch, während eine HTTP-Anfrage wartet.
max_wall_time_ms = 10000
fuel_slice       = 1000000   # Instruktionen je Zeitscheibe; danach prüft der Host auf Abbruch
# Deckel für host_http (nur mit net): Anfragen je Werkzeugaufruf, größte Antwort in Bytes,
# Zeitbudget je Anfrage in ms (wird auf die Rest-Wanduhr gekappt).
# max_http_requests       = 16
# max_http_response_bytes = 4194304
# http_timeout_ms         = 10000
"#;

const README_TEMPLATE: &str = r#"# {{name}} — ein Plugin für sepp mini

Erzeugt von `sepp plugin new`. Die Arbeit steht in `src/lib.rs`; das Aufrufprotokoll übernimmt
das SDK `sepp-plugin`.

## Bauen und testen

```bash
cargo test                                             # nativ, ohne wasm32-Target
rustup target add wasm32-unknown-unknown               # einmalig
cargo build --release --target wasm32-unknown-unknown
```

Das Modul liegt danach unter `target/wasm32-unknown-unknown/release/{{name}}.wasm`.

## Installieren

```bash
cp target/wasm32-unknown-unknown/release/{{name}}.wasm ~/.sepp/plugins/
cp {{name}}.toml ~/.sepp/plugins/
```

Beim nächsten Start meldet sepp `WASM: 1 Plugins geladen`; `sepp policy` führt den Akteur
`plugin {{name}}` auf. Erweiterungen werden nur beim Start gelesen — nach jeder Änderung sepp neu
starten.

## Rechte

Ohne `[capabilities]` im Manifest braucht das Plugin keinen Eintrag in der `policy.toml`. Sobald
es etwas anfordert, muss der Nutzer gegenzeichnen, zum Beispiel:

```bash
sepp policy allow --global plugin.{{name}} fs_read ./daten
```

Drei Stellen müssen zusammenpassen: das Cargo-Feature am SDK (`fs-read` / `net`), der Eintrag in
`{{name}}.toml` und die Gewährung in der Policy. Effektiv gilt der Schnitt aus Manifest und Policy;
ein Modul, das eine Funktion ohne Gewährung importiert, lädt nicht.

Vertragstext der Schnittstelle: `wit/sepp.wit` im sepp-mini-Repo. Vollständiges Beispiel:
`examples/textstat-plugin` dort.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn a(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_forms_and_errors() {
        assert_eq!(
            parse_plugin_args(&a(&["new", "demo"])).unwrap(),
            PluginCmd::New {
                name: "demo".into(),
                dir: None,
                sdk_path: None
            }
        );
        assert_eq!(
            parse_plugin_args(&a(&["new", "--dir", "/tmp/x", "demo", "--sdk-path", "sdk"]))
                .unwrap(),
            PluginCmd::New {
                name: "demo".into(),
                dir: Some(PathBuf::from("/tmp/x")),
                sdk_path: Some(PathBuf::from("sdk"))
            }
        );
        for bad in [
            vec![],
            vec!["bogus"],
            vec!["new"],
            vec!["new", "a", "b"],
            vec!["new", "--dir"],
            vec!["new", "demo", "--sdk-path"],
            vec!["new", "demo", "--bogus"],
        ] {
            assert!(parse_plugin_args(&a(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn validate_name_is_strict() {
        for ok in ["demo", "text_stat2", "a", &"x".repeat(64)] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "Demo",
            "1abc",
            "a-b",
            "fn",
            "test",
            "sepp_plugin",
            "grüße",
            "mit leerzeichen",
            &"x".repeat(65),
        ] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn templates_leave_no_placeholders() {
        let sdk = sdk_dep_git("9.9.9");
        for t in [
            CARGO_TEMPLATE,
            LIB_TEMPLATE,
            MANIFEST_TEMPLATE,
            README_TEMPLATE,
        ] {
            let out = render(t, "demo", &sdk);
            assert!(!out.contains("{{"), "{out}");
            assert!(out.contains("demo"));
        }
    }

    #[test]
    fn manifest_template_parses_as_a_valid_manifest() {
        let m = sepp_policy::Manifest::parse(&render(MANIFEST_TEMPLATE, "demo", "")).unwrap();
        assert_eq!(m.name, "demo");
        assert_eq!(m.abi, 1);
        assert!(m.unknown_keys().is_empty(), "{:?}", m.unknown_keys());
        assert!(m.capabilities.fs_read.is_empty());
        assert_eq!(m.limits.max_wall_time_ms, 10_000);
        assert!(m.limits.validate().is_ok());
    }

    #[test]
    fn cargo_template_is_toml_with_a_cdylib_and_the_sdk() {
        let git: toml::Value =
            toml::from_str(&render(CARGO_TEMPLATE, "demo", &sdk_dep_git("0.9.0")))
                .expect("Cargo.toml parst");
        assert_eq!(git["package"]["name"].as_str(), Some("demo"));
        assert_eq!(git["lib"]["name"].as_str(), Some("demo"));
        assert_eq!(git["lib"]["crate-type"][0].as_str(), Some("cdylib"));
        assert_eq!(
            git["dependencies"]["sepp-plugin"]["tag"].as_str(),
            Some("v0.9.0")
        );
        assert_eq!(
            git["dependencies"]["sepp-plugin"]["git"].as_str(),
            Some(SDK_GIT)
        );

        let path: toml::Value = toml::from_str(&render(
            CARGO_TEMPLATE,
            "demo",
            &sdk_dep_path(Path::new("/x/y")),
        ))
        .unwrap();
        assert_eq!(
            path["dependencies"]["sepp-plugin"]["path"].as_str(),
            Some("/x/y")
        );
        assert!(path["dependencies"]["sepp-plugin"].get("git").is_none());
    }

    #[test]
    fn scaffold_never_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("demo");
        scaffold(&dir, "demo", "path = \"/x\"").unwrap();
        for f in [
            "Cargo.toml",
            "src/lib.rs",
            "demo.toml",
            "README.md",
            ".gitignore",
        ] {
            assert!(dir.join(f).is_file(), "{f}");
        }
        // Zweiter Lauf: nichts passiert, nichts bricht.
        scaffold(&dir, "demo", "path = \"/x\"").unwrap();
        // Eine geänderte Datei bleibt beim dritten Lauf, wie sie ist.
        std::fs::write(dir.join("src/lib.rs"), "// meins").unwrap();
        scaffold(&dir, "demo", "path = \"/x\"").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "// meins"
        );
    }

    #[test]
    fn new_plugin_rejects_bad_names_and_missing_sdk_paths() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(new_plugin("Demo", Some(tmp.path().join("x")), None).is_err());
        assert!(
            !tmp.path().join("x").exists(),
            "bei ungültigem Namen wird nichts angelegt"
        );
        let e = new_plugin(
            "demo",
            Some(tmp.path().join("x")),
            Some(tmp.path().join("kein-sdk")),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("--sdk-path"), "{e}");
    }

    /// Das Gerüst baut wirklich — nativ (`cargo test` im Gerüst) und für wasm32 — und lädt im
    /// Host. `#[ignore]`, weil die CI kein WASM-Target hat; Aufruf:
    ///
    /// ```bash
    /// cargo test -p sepp-cli -- --ignored scaffold_builds
    /// ```
    #[test]
    #[ignore = "braucht das Target wasm32-unknown-unknown"]
    fn scaffold_builds_tests_and_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("demo");
        let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../sepp-plugin");
        new_plugin("demo", Some(dir.clone()), Some(sdk)).expect("Gerüst");

        let run = |args: &[&str]| {
            let out = std::process::Command::new("cargo")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("cargo startbar");
            assert!(
                out.status.success(),
                "cargo {args:?} scheitert:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["test", "--quiet"]);
        run(&["build", "--release", "--target", "wasm32-unknown-unknown"]);

        let wasm = dir.join("target/wasm32-unknown-unknown/release/demo.wasm");
        use sepp_tools::Tool as _;
        let plugin = sepp_wasm::WasmHost::new()
            .load_file_with_grant(&wasm, Some(&dir.join("demo.toml")), None)
            .expect("Gerüst-Plugin lädt ohne Gewährung");
        assert_eq!(plugin.spec().name, "demo");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt
            .block_on(sepp_tools::Tool::execute(
                &plugin,
                serde_json::json!({ "text": "hallo du" }),
                tokio_util::sync::CancellationToken::new(),
                None,
            ))
            .unwrap();
        assert!(!res.is_error, "{res:?}");
        assert_eq!(res.details["words"], 2);
    }

    /// Ein Gerüst mit Feature `net` spricht über `host_http` mit einem lokalen Listener — der
    /// ganze Weg: SDK-Builder → Modul → Host-Allowlist → Leitung → Antwort → `details`. Ebenfalls
    /// `#[ignore]` (wasm32-Target).
    #[test]
    #[ignore = "braucht das Target wasm32-unknown-unknown"]
    fn scaffold_with_net_talks_to_a_local_listener() {
        use std::io::{Read, Write};

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("netdemo");
        let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../sepp-plugin");
        new_plugin("netdemo", Some(dir.clone()), Some(sdk)).expect("Gerüst");

        // Feature `net` am SDK, ein Werkzeug, das die URL aus den Argumenten holt, und ein
        // Manifest, das 127.0.0.1 anfordert.
        let cargo = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        let cargo = cargo.replace(
            "sepp-plugin = { path",
            "sepp-plugin = { features = [\"net\"], path",
        );
        assert!(cargo.contains("features = [\"net\"]"), "{cargo}");
        std::fs::write(dir.join("Cargo.toml"), cargo).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            r##"use sepp_plugin::prelude::*;

#[derive(Deserialize, JsonSchema)]
struct Args { url: String }

#[sepp_plugin::tool(desc = "Holt eine URL über den Host.")]
fn netdemo(args: Args, host: &Host) -> Result<ToolResult> {
    let resp = host.http().get(&args.url).header("X-Von", "netdemo").send()?;
    Ok(ToolResult::text(resp.text()?).with_details(json!({ "status": resp.status })))
}
"##,
        )
        .unwrap();
        std::fs::write(
            dir.join("netdemo.toml"),
            "name = \"netdemo\"\nabi = 1\n[capabilities]\nnet = [\"127.0.0.1\"]\n",
        )
        .unwrap();

        let out = std::process::Command::new("cargo")
            .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
            .current_dir(&dir)
            .output()
            .expect("cargo startbar");
        assert!(
            out.status.success(),
            "Netz-Gerüst baut nicht:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );

        // Ein Listener, der genau eine Anfrage beantwortet und den Kopf festhält.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
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
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nservus");
            String::from_utf8_lossy(&head).to_lowercase()
        });

        use sepp_tools::Tool as _;
        let grant = sepp_policy::Policy::new(vec![sepp_policy::Capability::Net {
            host: "127.0.0.1".into(),
        }]);
        let plugin = sepp_wasm::WasmHost::new()
            .load_file_with_grant(
                &dir.join("target/wasm32-unknown-unknown/release/netdemo.wasm"),
                Some(&dir.join("netdemo.toml")),
                Some(&grant),
            )
            .expect("Netz-Gerüst lädt mit Gewährung");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt
            .block_on(plugin.execute(
                serde_json::json!({ "url": format!("http://{addr}/hallo") }),
                tokio_util::sync::CancellationToken::new(),
                None,
            ))
            .unwrap();
        assert!(!res.is_error, "{res:?}");
        assert_eq!(res.details["status"], 200);
        let sepp_core::ContentBlock::Text { text } = &res.content[0] else {
            panic!("{res:?}")
        };
        assert_eq!(text, "servus");
        assert_eq!(res.details["audit"]["kind"], "plugin_http");
        assert_eq!(res.details["audit"]["requests"][0]["status"], 200);
        let head = seen.join().unwrap();
        assert!(head.contains("get /hallo http/1.1"), "{head}");
        assert!(head.contains("x-von: netdemo"), "{head}");
    }
}
