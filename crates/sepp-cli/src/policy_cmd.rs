//! `sepp policy` — zeigt das effektive Regelwerk (Sepp Guard) je Akteur samt **Vollstrecker** und
//! benennt, was auf diesem System nicht durchsetzbar ist. Dazu das `policy.toml`-Template und die
//! Preset-Erkennung für `sepp init` sowie der Hinweis-Stub für `sepp policy allow …`.

use std::path::Path;
use std::process::ExitCode;

use sepp_policy::{
    kernel_capabilities, load_policy_set, Actor, BuiltinDefaults, Capability, Manifest, Mode,
    Policy, PolicySet, ResolveCtx, SandboxCapabilities, Source,
};

use crate::session;

/// Unterbefehle von `sepp policy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyCmd {
    Show,
    Allow(Vec<String>),
}

/// `sepp policy [show | allow [--global] <akteur> <recht> <wert>]`.
pub fn parse_policy_args(args: &[String]) -> Result<PolicyCmd, String> {
    match args.first().map(String::as_str) {
        None | Some("show") => Ok(PolicyCmd::Show),
        Some("allow") => Ok(PolicyCmd::Allow(args[1..].to_vec())),
        Some(other) => Err(format!(
            "policy: unbekannter Unterbefehl '{other}' (erlaubt: show, allow)"
        )),
    }
}

/// Zerlegt die `allow`-Argumente: optionales `--global`, dann Akteur, Recht, Wert.
/// Reine Funktion (testbar ohne Dateisystem).
pub fn parse_allow_args(args: &[String]) -> Result<(bool, Actor, String, String), String> {
    let usage = "Verwendung: sepp policy allow [--global] <agent | mcp.<name> | plugin.<name>> \
                 <fs_read | fs_write | net | env | exec> <wert>";
    let mut global = false;
    let mut rest: Vec<&String> = Vec::new();
    for a in args {
        match a.as_str() {
            "--global" | "-g" => global = true,
            other if other.starts_with('-') => {
                return Err(format!(
                    "policy allow: unbekannte Option '{other}'\n{usage}"
                ))
            }
            _ => rest.push(a),
        }
    }
    let [actor_raw, right, value] = rest.as_slice() else {
        return Err(usage.to_string());
    };
    let actor = sepp_policy::policy_edit::parse_actor(actor_raw)
        .ok_or_else(|| format!("policy allow: unbekannter Akteur '{actor_raw}'\n{usage}"))?;
    if !sepp_policy::policy_edit::RIGHTS.contains(&right.as_str()) {
        return Err(format!(
            "policy allow: unbekanntes Recht '{right}'\n{usage}"
        ));
    }
    Ok((global, actor, right.to_string(), value.to_string()))
}

/// Transport eines MCP-Servers — entscheidet über den Vollstrecker (`http` = kein Sandboxing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Stdio,
    Http,
}

/// Akteure, die das Frontend zusätzlich kennt: MCP-Server aus `settings.toml` (samt
/// Legacy-Rechten aus `[mcp.servers.capabilities]`) und Plugins aus den Plugin-Verzeichnissen.
#[derive(Debug, Clone)]
pub struct ActorRow {
    pub actor: Actor,
    pub transport: Option<Transport>,
    pub legacy: Option<Policy>,
    /// Nutzt dieser Server `$NAME`-Platzhalter in `[mcp.servers.headers]`? Dann sind `net` und
    /// `env` für ihn **nicht** wirkungslos, auch wenn er remote läuft — der Secret-Broker setzt
    /// beide durch, bevor er verbindet.
    pub secret_headers: bool,
}

pub fn run_policy(cmd: PolicyCmd) -> ExitCode {
    match cmd {
        PolicyCmd::Allow(args) => match run_allow(&args) {
            Ok(msg) => {
                println!("{msg}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("Fehler: {e}");
                ExitCode::from(2)
            }
        },
        PolicyCmd::Show => match show() {
            Ok(s) => {
                println!("{s}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("Fehler: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

fn show() -> anyhow::Result<String> {
    let trust = session::project_trust_state().unwrap_or(session::TrustState::Untrusted);
    let trusted = trust == session::TrustState::Trusted;
    let sources = session::policy_paths(trusted)?;
    let defaults = BuiltinDefaults {
        extra_deny: session::builtin_deny_roots()?,
        extra_deny_write: vec![session::project_root()?],
        default_mode: Mode::Ask,
    };
    let mode_override = std::env::var("SEPP_MODE")
        .ok()
        .and_then(|v| v.parse::<Mode>().ok());
    let set = load_policy_set(&sources, &defaults, mode_override, &ResolveCtx::from_env())?;
    let caps = kernel_capabilities();

    let rows = actor_rows(trusted)?;
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    let mut out = render_policy_table(&set, &caps, &rows, &cwd, trusted);
    if let session::TrustState::Changed { expected, actual } = &trust {
        out.push_str(&format!(
            "\nHinweis: Vertrauen ins Projekt ausgesetzt — seine Konfiguration (.sepp/: policy.toml, \
             settings.toml, hooks, plugins) hat sich seit /trust geändert (Stand {} → {}). Sie \
             wird nicht geladen, bis sie erneut bestätigt ist: /trust in der TUI.\n",
            session::short_fingerprint(expected),
            session::short_fingerprint(actual)
        ));
    }
    Ok(out)
}

/// MCP-Server und Plugins als Zeilen der Rechteübersicht. Von `sepp policy` **und** vom
/// TUI-Befehl `/policy` genutzt — sonst zeigte die TUI ein anderes Bild als das Terminal.
pub fn actor_rows(trusted: bool) -> anyhow::Result<Vec<ActorRow>> {
    let mut rows: Vec<ActorRow> = Vec::new();
    for cfg in sepp_mcp::load_settings(&session::settings_paths(trusted)?)? {
        let transport = if cfg.transport == "http" {
            Transport::Http
        } else {
            Transport::Stdio
        };
        let legacy = sepp_mcp::policy_from_config(&cfg);
        let secret_headers = cfg
            .headers
            .values()
            .any(|v| !sepp_policy::placeholder_names(v).is_empty());
        rows.push(ActorRow {
            actor: Actor::Mcp(cfg.name),
            transport: Some(transport),
            legacy: (!legacy.granted.is_empty()).then_some(legacy),
            secret_headers,
        });
    }
    for dir in session::plugin_dirs(trusted)? {
        for name in plugin_names(&dir) {
            rows.push(ActorRow {
                actor: Actor::Plugin(name),
                transport: None,
                legacy: None,
                secret_headers: false,
            });
        }
    }
    Ok(rows)
}

/// Plugin-Namen eines Plugin-Verzeichnisses ohne WASM-Compile: Manifest-`name`, sonst Dateistamm.
fn plugin_names(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("wasm") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string();
        let stem_manifest = path.with_extension("toml");
        let dir_manifest = path.with_file_name("manifest.toml");
        let manifest = [stem_manifest, dir_manifest]
            .into_iter()
            .find(|p| p.exists());
        let name = manifest
            .and_then(|m| Manifest::from_file(&m).ok())
            .map(|m| m.name)
            .unwrap_or(stem);
        out.push(name);
    }
    out.sort();
    out
}

fn cap_kind(cap: &Capability) -> &'static str {
    match cap {
        Capability::FsRead { .. } => "fs_read",
        Capability::FsWrite { .. } => "fs_write",
        Capability::Net { .. } => "net",
        Capability::Env { .. } => "env",
        Capability::Exec { .. } => "exec",
    }
}

fn cap_value(cap: &Capability) -> String {
    match cap {
        Capability::FsRead { prefix } | Capability::FsWrite { prefix } => {
            prefix.display().to_string()
        }
        Capability::Net { host } => {
            if host == "*" {
                "true".into()
            } else {
                host.clone()
            }
        }
        Capability::Env { name } => name.clone(),
        Capability::Exec { program } => program.clone(),
    }
}

/// Quelle kurz: projektlokale Pfade relativ zum Arbeitsverzeichnis (`./.sepp/policy.toml`),
/// Pfade unter `$HOME` mit `~`.
fn short_source(src: &Source) -> String {
    match src {
        Source::Builtin => "eingebaut".into(),
        Source::Cli => "--mode/SEPP_MODE".into(),
        Source::File(p) => {
            if let Ok(cwd) = std::env::current_dir() {
                if let Ok(rel) = p.strip_prefix(&cwd) {
                    return format!("./{}", rel.display());
                }
            }
            let s = p.display().to_string();
            match std::env::var("HOME") {
                Ok(home) if !home.is_empty() && s.starts_with(&home) => {
                    format!("~{}", &s[home.len()..])
                }
                _ => s,
            }
        }
    }
}

/// Fügt eine Zeile an, sofern nicht bereits eine identische existiert (das Schlüsselwort
/// `system` expandiert zu mehreren Pfaden mit demselben Rohtext).
fn push_row(rows: &mut Vec<[String; 5]>, row: [String; 5]) {
    if !rows.contains(&row) {
        rows.push(row);
    }
}

fn fs_enforcer(caps: &SandboxCapabilities) -> &'static str {
    if cfg!(target_os = "macos") {
        "Seatbelt"
    } else if caps.fs_enforceable {
        "Landlock"
    } else {
        "NICHT durchsetzbar"
    }
}

fn net_deny_enforcer(caps: &SandboxCapabilities, transport: Option<Transport>) -> String {
    match transport {
        Some(Transport::Http) => "KEINE (remote)".into(),
        _ => {
            if cfg!(target_os = "macos") {
                "Seatbelt (kein network*)".into()
            } else if caps.net_enforceable {
                "Landlock TCP-Verbot".into()
            } else {
                "NICHT durchsetzbar (Kernel < 6.7)".into()
            }
        }
    }
}

fn enforcer(
    actor: &Actor,
    cap: &Capability,
    caps: &SandboxCapabilities,
    transport: Option<Transport>,
    net_denied: bool,
    secret_headers: bool,
) -> String {
    let fs = fs_enforcer(caps);
    // Ein Netzverbot nimmt die Gewährung zurück. Die Zeile bleibt sichtbar — sonst sähe man
    // nicht, dass jemand sie eingetragen hat — aber sie darf nicht „erlaubt" behaupten.
    if net_denied && matches!(cap, Capability::Net { .. }) {
        return "zurückgenommen durch [deny] net".into();
    }
    match actor {
        Actor::Agent => match cap {
            Capability::FsRead { .. } | Capability::FsWrite { .. } => {
                format!("{fs} (bash) · Pfadprüfung (read/write/edit)")
            }
            Capability::Net { .. } => {
                "TCP gesamt erlaubt (Host-Filter folgt mit Egress-Proxy)".into()
            }
            Capability::Env { .. } => "env_clear + Allowlist (bash)".into(),
            Capability::Exec { .. } => format!("Execute-Allowlist ({fs})"),
        },
        Actor::Mcp(_) => match transport {
            // Für einen remote laufenden Server gibt es keinen Vollstrecker — mit einer
            // Ausnahme: Sobald seine Header ein Secret verlangen, entscheiden `net` und `env`
            // darüber, ob überhaupt verbunden wird und welcher Wert mitgeht.
            Some(Transport::Http)
                if secret_headers
                    && matches!(cap, Capability::Net { .. } | Capability::Env { .. }) =>
            {
                "Secret-Broker (Header, vor dem Verbinden)".into()
            }
            Some(Transport::Http) => "KEINE (remote)".into(),
            _ => match cap {
                Capability::FsRead { .. } | Capability::FsWrite { .. } => fs.into(),
                Capability::Net { .. } => "TCP gesamt erlaubt (Host-Filter folgt)".into(),
                Capability::Env { .. } => "env_clear + Allowlist".into(),
                Capability::Exec { .. } => format!("Execute-Allowlist ({fs})"),
            },
        },
        // Ein Plugin hat weder Sockets noch Dateisystem — nur Host-Funktionen, die der Linker
        // erst bei Gewährung registriert und die dann je Aufruf selbst prüfen. Für `net` ist
        // sepp der Netzwerkstack des Moduls: Die Host-Allowlist gilt dort exakt, ohne
        // Egress-Proxy.
        Actor::Plugin(_) => match cap {
            Capability::FsRead { .. } => {
                "wasmi-Linker-Gate + Pfadprüfung je Aufruf (host_fs_read)".into()
            }
            Capability::FsWrite { .. } => {
                "wasmi-Linker-Gate (schließt Lesen ein; kein Schreib-Import)".into()
            }
            Capability::Net { .. } => {
                "wasmi-Linker-Gate + Host-Allowlist je Anfrage (host_http); Secrets: Broker".into()
            }
            Capability::Env { .. } => {
                "Secret-Broker ($NAME in host_http-Headern, nur mit net)".into()
            }
            Capability::Exec { .. } => "wasmi-Linker-Gate (kein Host-Import)".into(),
        },
    }
}

/// Reine Tabellen-Ausgabe (testbar): Akteur | Recht | Wert | Quelle | Vollstrecker, plus Verbote,
/// Rückfrage-Muster und die Zeile „Nicht durchsetzbar auf diesem System".
pub fn render_policy_table(
    set: &PolicySet,
    caps: &SandboxCapabilities,
    extra: &[ActorRow],
    cwd: &str,
    trusted: bool,
) -> String {
    let mut out = String::new();
    out.push_str("Sepp Guard — effektives Regelwerk\n");
    out.push_str(&format!(
        "Modus:   {}  (Quelle: {})\n",
        set.mode,
        short_source(&set.mode_source)
    ));
    out.push_str(&format!(
        "Projekt: {cwd}  ({})\n",
        if trusted {
            "vertraut"
        } else {
            "nicht vertraut — projektlokale .sepp/policy.toml wird nicht geladen; /trust oder sepp init"
        }
    ));
    out.push_str("Quellen:\n");
    if set.sources.is_empty() {
        out.push_str("  (keine Dateien)\n");
    }
    for (src, found) in &set.sources {
        out.push_str(&format!(
            "  {}{}  {}\n",
            short_source(&Source::File(src.path.clone())),
            if matches!(src.kind, sepp_policy::SourceKind::SettingsToml) {
                " [policy]"
            } else {
                ""
            },
            if *found { "geladen" } else { "nicht vorhanden" }
        ));
    }
    out.push_str(&format!("Sandbox: {}\n\n", caps.detail));

    // Akteure: Agent zuerst, dann die aus dem Regelwerk und die vom Frontend gemeldeten.
    let mut actors: Vec<Actor> = vec![Actor::Agent];
    for a in set.actors() {
        if !actors.contains(&a) {
            actors.push(a);
        }
    }
    for r in extra {
        if !actors.contains(&r.actor) {
            actors.push(r.actor.clone());
        }
    }
    let net_denied = set.deny_net.is_some();

    let mut rows: Vec<[String; 5]> = Vec::new();
    for actor in &actors {
        let row_info = extra.iter().find(|r| r.actor == *actor);
        let transport = row_info.and_then(|r| r.transport);
        let label = actor.to_string();
        let mut has_net = false;
        let mut has_exec = false;
        // Dieselbe Prädikatsfunktion wie der Loader (`main.rs`, `grant_for`): Sie zählt auch
        // `exec_open` mit. Eine eigene Zählung über `entries` allein meldete einen Abschnitt
        // mit nur `exec = "system"` als nicht vorhanden — die Übersicht widerspräche dem, was
        // beim Laden tatsächlich passiert.
        let any = set.has_entries(actor);
        for e in set.entries.iter().filter(|e| e.actor == *actor) {
            has_net |= matches!(e.cap, Capability::Net { .. });
            has_exec |= matches!(e.cap, Capability::Exec { .. });
            push_row(
                &mut rows,
                [
                    label.clone(),
                    cap_kind(&e.cap).into(),
                    e.raw.clone(),
                    short_source(&e.source),
                    enforcer(
                        actor,
                        &e.cap,
                        caps,
                        transport,
                        net_denied,
                        row_info.is_some_and(|r| r.secret_headers),
                    ),
                ],
            );
        }
        // Der alte capabilities-Block wird nur noch angezeigt, nicht mehr durchgesetzt. Er zählt
        // deshalb auch nicht als Gewährung (`any` bleibt unberührt), sonst verschwände die Zeile
        // „net aus" für einen Server, der faktisch kein Netz hat.
        if let Some(legacy) = row_info.and_then(|r| r.legacy.as_ref()) {
            for c in &legacy.granted {
                rows.push([
                    label.clone(),
                    cap_kind(c).into(),
                    cap_value(c),
                    "settings.toml (veraltet)".into(),
                    format!(
                        "wirkungslos — gehört nach [{}] in die policy.toml",
                        section_label(actor)
                    ),
                ]);
            }
        }
        match actor {
            Actor::Plugin(_) => {
                if !any {
                    rows.push([
                        label.clone(),
                        "(kein Abschnitt)".into(),
                        "keine Rechte".into(),
                        "–".into(),
                        "wasmi-Linker-Gate (lädt nicht, wenn es eine gegatete Host-Funktion importiert)"
                            .into(),
                    ]);
                } else if !has_net {
                    rows.push([
                        label.clone(),
                        "net".into(),
                        "aus".into(),
                        "eingebaut".into(),
                        "wasmi-Linker-Gate (host_http nicht registriert; lädt nicht, wenn importiert)".into(),
                    ]);
                }
            }
            _ => {
                if !has_net {
                    rows.push([
                        label.clone(),
                        "net".into(),
                        "aus".into(),
                        "eingebaut".into(),
                        net_deny_enforcer(caps, transport),
                    ]);
                }
                if !has_exec || set.exec_open.contains(actor) {
                    rows.push([
                        label.clone(),
                        "exec".into(),
                        "system (unbeschränkt)".into(),
                        "eingebaut".into(),
                        if matches!(transport, Some(Transport::Http)) {
                            "KEINE (remote)".into()
                        } else {
                            "–".into()
                        },
                    ]);
                }
            }
        }
    }

    let header = ["AKTEUR", "RECHT", "WERT", "QUELLE", "VOLLSTRECKER"];
    let mut widths = [0usize; 5];
    for (i, h) in header.iter().enumerate() {
        widths[i] = h.chars().count();
    }
    for r in &rows {
        for (i, cell) in r.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let fmt_row = |cells: [&str; 5]| -> String {
        let mut line = String::new();
        for (i, c) in cells.iter().enumerate() {
            line.push_str(c);
            if i + 1 < cells.len() {
                let pad = widths[i].saturating_sub(c.chars().count()) + 2;
                line.push_str(&" ".repeat(pad));
            }
        }
        line.trim_end().to_string()
    };
    out.push_str(&fmt_row(header));
    out.push('\n');
    for r in &rows {
        out.push_str(&fmt_row([&r[0], &r[1], &r[2], &r[3], &r[4]]));
        out.push('\n');
    }

    out.push_str("\nVerbote (gewinnen immer):\n");
    if let Some(src) = &set.deny_net {
        out.push_str(&format!(
            "  net  [jeder Akteur, jede Quelle]  ({})\n",
            short_source(src)
        ));
    }
    for (rule, src) in &set.deny {
        out.push_str(&format!(
            "  {}  [{}]  ({})\n",
            rule.prefix.display(),
            match rule.kind {
                sepp_policy::DenyKind::Read => "lesen+schreiben",
                sepp_policy::DenyKind::Write => "schreiben",
            },
            short_source(src)
        ));
    }
    if !set.ask_patterns.is_empty() {
        out.push_str("Rückfrage-Muster (Modus ask):\n");
        for (p, src) in &set.ask_patterns {
            out.push_str(&format!("  {p:?}  ({})\n", short_source(src)));
        }
    }

    let mut unenforceable: Vec<String> = Vec::new();
    if !caps.fs_enforceable {
        unenforceable.push(format!(
            "Dateisystem-Sandbox für Kindprozesse ({})",
            caps.detail
        ));
    } else if !caps.net_enforceable {
        unenforceable.push("TCP-Verbot für Kindprozesse (Kernel < 6.7 / Landlock ABI < 4)".into());
    }
    // Das eingebaute Schreibverbot auf <cwd>/.sepp liegt immer unter der Gewährung ./ — für bash
    // greift dort nicht Landlock, sondern die Bindung des Vertrauens an den Inhalt.
    let project_cfg = sepp_policy::canonicalize_lenient(&std::path::Path::new(cwd).join(".sepp"));
    for actor in &actors {
        for o in set.deny_overlaps(actor) {
            if o.deny == project_cfg {
                unenforceable.push(format!(
                    "Schreibverbot {} ({}): für bash nicht durchsetzbar — eine Änderung dort hebt das Vertrauen ins Projekt auf, bis /trust es neu bestätigt",
                    o.deny.display(),
                    actor
                ));
                continue;
            }
            unenforceable.push(format!(
                "Verbot {} unter Gewährung {} ({}): für Kindprozesse nicht durchsetzbar, In-Process-Prüfung greift",
                o.deny.display(),
                o.grant.display(),
                actor
            ));
        }
    }
    // Für Plugins gilt die Host-Liste exakt (`host_http` ist der Netzwerkstack des Moduls);
    // nur für Agent und MCP-Kindprozesse ist Netz ganz oder gar nicht, bis der Egress-Proxy da ist.
    if set.deny_net.is_none()
        && set.entries.iter().any(|e| {
            !matches!(e.actor, Actor::Plugin(_))
                && matches!(&e.cap, Capability::Net { host } if host != "*")
        })
    {
        unenforceable.push(
            "Host-Filter für net-Listen bei agent/mcp (TCP gesamt erlaubt; Egress-Proxy folgt)"
                .into(),
        );
    }
    for w in &set.warnings {
        unenforceable.push(w.clone());
    }
    out.push_str("\nNicht durchsetzbar auf diesem System: ");
    if unenforceable.is_empty() {
        out.push_str("keine\n");
    } else {
        out.push('\n');
        for u in unenforceable {
            out.push_str(&format!("  - {u}\n"));
        }
    }
    out
}

/// `sepp policy allow [--global] <akteur> <recht> <wert>` — trägt das Recht in die Policy-Datei
/// ein (projektlokal `<cwd>/.sepp/policy.toml`, mit `--global` in `<config_root>/policy.toml`).
/// Kommentare und Formatierung der Datei bleiben erhalten; ein vorhandener Wert ist ein No-op.
fn run_allow(args: &[String]) -> anyhow::Result<String> {
    let (global, actor, right, value) =
        parse_allow_args(args).map_err(|e| anyhow::anyhow!("{e}"))?;
    let path = if global {
        session::config_root()?.join("policy.toml")
    } else {
        session::project_policy_path()?
    };
    // Vor dem Schreiben festhalten, ob das Vertrauen bis hierher galt — danach ist der Stand
    // durch unseren eigenen Eintrag ohnehin ein anderer.
    let before = if global {
        None
    } else {
        Some(session::project_trust_state()?)
    };
    let written = sepp_policy::policy_edit::allow(&path, &actor, &right, &value)?;
    let mut out = if written {
        format!(
            "Eingetragen in {}: [{}] {right} = \"{value}\"",
            path.display(),
            section_label(&actor)
        )
    } else {
        format!("Stand bereits in {}: {right} = \"{value}\"", path.display())
    };
    if let Some(before) = before {
        match before {
            session::TrustState::Trusted => {
                // Die Änderung hat der Mensch veranlasst — das Vertrauen folgt dem neuen Stand.
                session::rebind_trust_after_own_write(&before)?;
                out.push_str(
                    "\nVertrauen ins Projekt an den neuen Stand der Konfiguration gebunden.",
                );
            }
            session::TrustState::Changed { .. } => out.push_str(
                "\nHinweis: Die Konfiguration hatte sich schon vor diesem Eintrag seit /trust \
                 geändert — das Vertrauen bleibt ausgesetzt, bis /trust in der TUI (oder \
                 sepp init) den gesamten Stand bestätigt.",
            ),
            session::TrustState::Untrusted => out.push_str(
                "\nHinweis: Das Projekt ist noch nicht vertraut — die Datei wirkt erst nach \
                 `sepp init` bzw. `/trust` in der TUI.",
            ),
        }
    }
    out.push_str("\nWirksam beim nächsten Start. Kontrolle: sepp policy");
    Ok(out)
}

/// TOML-Abschnittsname eines Akteurs (`agent`, `mcp.git`, `plugin.string-tools`).
fn section_label(actor: &Actor) -> String {
    match actor {
        Actor::Agent => "agent".into(),
        Actor::Mcp(n) => format!("mcp.{n}"),
        Actor::Plugin(n) => format!("plugin.{n}"),
    }
}

/// Projekttyp-Presets für `sepp init` (erkannt an Dateien im Projekt).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Rust,
    Node,
    Python,
}

impl Preset {
    fn name(self) -> &'static str {
        match self {
            Preset::Rust => "rust",
            Preset::Node => "node",
            Preset::Python => "python",
        }
    }

    /// Aktiver `[agent]`-Abschnitt des Presets (erweitert die eingebauten Defaults).
    fn section(self) -> &'static str {
        match self {
            Preset::Rust => "[agent]\nfs_read  = [\"~/.cargo\", \"~/.rustup\", \"~/.gitconfig\"]\nfs_write = [\"~/.cargo\"]\nnet      = true   # Paket-Downloads (crates.io, git); entfernen, wenn kein Netz gewünscht\nenv      = [\"CARGO_HOME\", \"RUSTUP_HOME\", \"RUSTUP_TOOLCHAIN\"]\n",
            Preset::Node => "[agent]\nfs_read  = [\"~/.npm\", \"~/.nvm\", \"~/.gitconfig\"]\nfs_write = [\"~/.npm\"]\nnet      = true   # Paket-Downloads (npm); entfernen, wenn kein Netz gewünscht\nenv      = [\"NPM_CONFIG_PREFIX\", \"NVM_DIR\"]\n",
            Preset::Python => "[agent]\nfs_read  = [\"~/.cache/pip\", \"~/.local/lib\", \"~/.gitconfig\"]\nfs_write = [\"~/.cache/pip\"]\nnet      = true   # Paket-Downloads (pip); entfernen, wenn kein Netz gewünscht\nenv      = [\"VIRTUAL_ENV\", \"PYTHONPATH\"]\n",
        }
    }
}

/// Erkennt den Projekttyp an Dateinamen im Projektverzeichnis (Rust vor Node vor Python).
pub fn select_preset(entries: &[String]) -> Option<Preset> {
    let has = |n: &str| entries.iter().any(|e| e == n);
    if has("Cargo.toml") {
        Some(Preset::Rust)
    } else if has("package.json") {
        Some(Preset::Node)
    } else if has("pyproject.toml") || has("requirements.txt") {
        Some(Preset::Python)
    } else {
        None
    }
}

/// Vorlage für `.sepp/policy.toml`: komplett kommentiert (parst zu „keine Änderung"); mit
/// `preset` wird der passende `[agent]`-Abschnitt aktiviert.
pub fn policy_template(preset: Option<Preset>) -> String {
    let mut s = String::from(
        r#"# sepp mini — Sepp Guard: Rechte des Agenten und der Erweiterungen.
# Projektlokal (lädt erst nach Trust: `sepp init` oder /trust); global: ~/.sepp/policy.toml
# oder der Abschnitt [policy] in ~/.sepp/settings.toml. `sepp policy` zeigt das Ergebnis.
#
# Diese Datei sagt, WAS ETWAS DARF. Die settings.toml sagt nur, was läuft.
#
# Die eingebauten Defaults gelten IMMER — Einträge hier erweitern sie, [deny] schränkt ein:
#   lesen:      ./ und Systempfade        schreiben: ./ und $TMPDIR
#   ausführen:  alle Systemprogramme       Netz:      aus (TCP verboten)
#   Umgebung:   PATH HOME LANG LC_ALL LC_CTYPE TERM TMPDIR
#   Verbote:    ~/.ssh ~/.aws ~/.gnupg ~/.sepp (+ config_root, state_root)
#
# mode = "ask"                  # ask | auto | yolo — CLI --mode und SEPP_MODE haben Vorrang
#
# [agent]                       # bash, read, write, edit und task
# fs_read  = ["~/.cargo"]       # Pfade: ~ und $TMPDIR werden expandiert, "system" = Systempfade
# fs_write = ["~/.cargo"]
# net      = true               # TCP erlauben (Host-Listen: TCP gesamt, Host-Filter folgt)
# env      = ["CARGO_HOME"]     # zusätzliche Umgebungsvariablen für bash
# exec     = ["sh", "cargo"]    # nur als Verschärfung: dann dürfen NUR diese Programme starten
#
# [agent.ask]                   # Rückfrage-Muster (Komfort, keine Sicherheitsgrenze)
# patterns = ["rm -rf", "git push --force"]
#
# [mcp.git]                     # die EINZIGE Rechtequelle für einen MCP-Server
# fs_write = ["./"]              # (settings.toml sagt nur, was läuft — nicht, was es darf)
# exec     = ["git"]
#
# [plugin.string-tools]         # Gewährung für ein WASM-Plugin: Schnitt mit dem Manifest.
# net      = ["api.example.com"] # Ohne Abschnitt bekommt ein Plugin NICHTS, und eines, das im
#                               # Manifest etwas fordert, lädt gar nicht.
#
# [deny]                        # gewinnt gegen jede Quelle und jeden Akteur
# fs_read  = ["~/.config/secrets"]   # sperrt Lesen UND Schreiben
# fs_write = ["~/.cargo/config.toml"] # sperrt nur Schreiben
# net      = true               # Hauptschalter: niemand kommt ins Netz (exec/env kann [deny] nicht)
#
# Presets (Kommentarzeichen entfernen, um eines zu aktivieren):
#   Rust:   [agent] fs_read=["~/.cargo","~/.rustup"] fs_write=["~/.cargo"] net=true env=["CARGO_HOME","RUSTUP_HOME"]
#   Node:   [agent] fs_read=["~/.npm","~/.nvm"]      fs_write=["~/.npm"]   net=true env=["NPM_CONFIG_PREFIX"]
#   Python: [agent] fs_read=["~/.cache/pip"]         fs_write=["~/.cache/pip"] net=true env=["VIRTUAL_ENV"]
"#,
    );
    if let Some(p) = preset {
        s.push_str(&format!(
            "\n# Preset \"{}\" aktiviert (sepp init hat den Projekttyp erkannt).\n{}",
            p.name(),
            p.section()
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use sepp_policy::{AgentSection, Grants, NetGrant, PolicyFile, PolicySource, SourceKind};
    use std::path::PathBuf;

    fn ctx() -> ResolveCtx {
        ResolveCtx {
            home: Some(PathBuf::from("/home/u")),
            cwd: PathBuf::from("/proj"),
            tmpdir: PathBuf::from("/tmp"),
        }
    }

    fn caps(net: bool) -> SandboxCapabilities {
        SandboxCapabilities {
            fs_enforceable: true,
            net_enforceable: net,
            scope_enforceable: true,
            detail: "Test-Sandbox".into(),
        }
    }

    #[test]
    fn parse_policy_args_variants() {
        assert_eq!(parse_policy_args(&[]).unwrap(), PolicyCmd::Show);
        assert_eq!(
            parse_policy_args(&["show".to_string()]).unwrap(),
            PolicyCmd::Show
        );
        assert_eq!(
            parse_policy_args(&["allow".to_string(), "agent".to_string()]).unwrap(),
            PolicyCmd::Allow(vec!["agent".to_string()])
        );
        assert!(parse_policy_args(&["bogus".to_string()]).is_err());
    }

    #[test]
    fn http_server_with_secret_headers_shows_the_broker_as_enforcer() {
        // Für einen remoten Server steht sonst „KEINE (remote)". Sobald seine Header ein Secret
        // verlangen, stimmt das für `net` und `env` nicht mehr — beide entscheiden dann, ob
        // überhaupt verbunden wird. Das ist der einzige Ort, an dem ein http-Server aufhört,
        // rechtlich ein Nullum zu sein.
        let mut file = PolicyFile::default();
        file.mcp.insert(
            "gh".into(),
            Grants {
                net: NetGrant::Hosts(vec!["api.example.com".into()]),
                env: vec!["TOKEN".into()],
                ..Grants::default()
            },
        );
        let set = PolicySet::merge(
            vec![(Source::File("/proj/.sepp/policy.toml".into()), file)],
            &BuiltinDefaults::default(),
            None,
            &ctx(),
        );
        let extra = vec![ActorRow {
            actor: Actor::Mcp("gh".into()),
            transport: Some(Transport::Http),
            legacy: None,
            secret_headers: true,
        }];
        let out = render_policy_table(&set, &caps(true), &extra, "/proj", true);
        assert!(out.contains("Secret-Broker (Header"), "{out}");

        // Ohne Secret-Header bleibt es bei der alten, richtigen Aussage.
        let plain = vec![ActorRow {
            actor: Actor::Mcp("gh".into()),
            transport: Some(Transport::Http),
            legacy: None,
            secret_headers: false,
        }];
        let out = render_policy_table(&set, &caps(true), &plain, "/proj", true);
        assert!(!out.contains("Secret-Broker"), "{out}");
        assert!(out.contains("KEINE (remote)"), "{out}");
    }

    #[test]
    fn exec_only_plugin_section_is_not_reported_as_missing() {
        // `exec = "system"` landet in `exec_open`, nicht in `entries`. Eine eigene Zählung über
        // `entries` allein meldete den Abschnitt als nicht vorhanden — während der Loader ihn
        // über `has_entries` sehr wohl sieht und dem Plugin eine (leere) Policy gibt.
        let mut file = PolicyFile::default();
        file.plugin.insert(
            "foo".into(),
            Grants {
                exec: sepp_policy::ExecGrant::System,
                ..Grants::default()
            },
        );
        let set = PolicySet::merge(
            vec![(Source::File("/proj/.sepp/policy.toml".into()), file)],
            &BuiltinDefaults::default(),
            None,
            &ctx(),
        );
        assert!(set.has_entries(&Actor::Plugin("foo".into())));

        let extra = vec![ActorRow {
            actor: Actor::Plugin("foo".into()),
            transport: None,
            legacy: None,
            secret_headers: false,
        }];
        let out = render_policy_table(&set, &caps(true), &extra, "/proj", true);
        let plugin_line = out
            .lines()
            .find(|l| l.contains("plugin.foo") || l.contains("foo"))
            .unwrap_or("");
        assert!(
            !plugin_line.contains("(kein Abschnitt)"),
            "die Übersicht widerspricht dem Loader: {out}"
        );
    }

    #[test]
    fn render_policy_table_lists_sources_enforcers_and_unenforceable_line() {
        let mut file = PolicyFile {
            agent: Some(AgentSection {
                grants: Grants {
                    fs_read: vec!["~/.cargo".into()],
                    net: NetGrant::Hosts(vec!["crates.io".into()]),
                    ..Grants::default()
                },
                ..AgentSection::default()
            }),
            ..PolicyFile::default()
        };
        file.mcp.insert(
            "git".into(),
            Grants {
                fs_write: vec!["./".into()],
                exec: sepp_policy::ExecGrant::Programs(vec!["git".into()]),
                ..Grants::default()
            },
        );
        let mut set = PolicySet::merge(
            vec![(Source::File("/proj/.sepp/policy.toml".into()), file)],
            &BuiltinDefaults::default(),
            None,
            &ctx(),
        );
        set.sources = vec![
            (
                PolicySource {
                    path: "/home/u/.sepp/settings.toml".into(),
                    kind: SourceKind::SettingsToml,
                },
                false,
            ),
            (
                PolicySource {
                    path: "/proj/.sepp/policy.toml".into(),
                    kind: SourceKind::PolicyToml,
                },
                true,
            ),
        ];
        let extra = vec![
            ActorRow {
                actor: Actor::Mcp("git".into()),
                transport: Some(Transport::Stdio),
                legacy: Some(Policy::new(vec![Capability::Env {
                    name: "GIT_DIR".into(),
                }])),
                secret_headers: false,
            },
            ActorRow {
                actor: Actor::Mcp("fpv7".into()),
                transport: Some(Transport::Http),
                legacy: None,
                secret_headers: false,
            },
            ActorRow {
                actor: Actor::Plugin("string-tools".into()),
                transport: None,
                legacy: None,
                secret_headers: false,
            },
        ];
        let out = render_policy_table(&set, &caps(true), &extra, "/proj", true);
        assert!(out.contains("VOLLSTRECKER"), "{out}");
        assert!(out.contains("nicht vorhanden"), "{out}");
        assert!(out.contains("geladen"), "{out}");
        // Eingebaute Defaults + Datei-Gewährung mit Quelle.
        assert!(out.contains("eingebaut"), "{out}");
        assert!(out.contains("~/.cargo"), "{out}");
        // `system` expandiert zu mehreren Pfaden, wird aber nur einmal gelistet.
        assert_eq!(
            out.lines()
                .filter(|l| l.contains("fs_read") && l.contains("system"))
                .count(),
            1,
            "{out}"
        );
        // Vollstrecker je Akteur.
        assert!(
            out.contains(&format!(
                "{} (bash) · Pfadprüfung",
                fs_enforcer(&caps(true))
            )),
            "{out}"
        );
        assert!(out.contains("KEINE (remote)"), "{out}");
        // Plugin ohne [plugin.<name>]: keine Rechte, und der Grund steht daneben.
        assert!(out.contains("(kein Abschnitt)"), "{out}");
        assert!(out.contains("lädt nicht, wenn es eine gegatete"), "{out}");
        assert!(out.contains("Execute-Allowlist"), "{out}");
        // Der alte capabilities-Block wird gezeigt, aber als wirkungslos ausgewiesen.
        assert!(out.contains("settings.toml (veraltet)"), "{out}");
        assert!(out.contains("gehört nach [mcp.git]"), "{out}");
        // Netz-Liste → Host-Filter nicht durchsetzbar.
        assert!(out.contains("Host-Filter für net-Listen"), "{out}");
        assert!(out.contains("Verbote (gewinnen immer)"), "{out}");
        assert!(out.contains("/home/u/.ssh"), "{out}");
        assert!(
            !out.contains("Nicht durchsetzbar auf diesem System: keine"),
            "{out}"
        );

        // Ohne Netz-Liste und mit voller Kernel-Unterstützung: „keine".
        let plain = PolicySet::merge(vec![], &BuiltinDefaults::default(), None, &ctx());
        let out = render_policy_table(&plain, &caps(true), &[], "/proj", false);
        assert!(
            out.contains("Nicht durchsetzbar auf diesem System: keine"),
            "{out}"
        );
        assert!(out.contains(&net_deny_enforcer(&caps(true), None)), "{out}");
        assert!(out.contains("nicht vertraut"), "{out}");
        // Kernel ohne Netz-Regeln → Zeile erscheint.
        let out = render_policy_table(&plain, &caps(false), &[], "/proj", false);
        assert!(out.contains("TCP-Verbot für Kindprozesse"), "{out}");
    }

    #[test]
    fn parse_allow_args_forms_and_errors() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (global, actor, right, value) =
            parse_allow_args(&a(&["agent", "fs_write", "/srv/data"])).unwrap();
        assert!(!global);
        assert_eq!(actor, Actor::Agent);
        assert_eq!((right.as_str(), value.as_str()), ("fs_write", "/srv/data"));

        let (global, actor, ..) =
            parse_allow_args(&a(&["--global", "mcp.git", "exec", "git"])).unwrap();
        assert!(global);
        assert_eq!(actor, Actor::Mcp("git".into()));

        // Reihenfolge der Option ist egal.
        assert!(
            parse_allow_args(&a(&["agent", "net", "true", "-g"]))
                .unwrap()
                .0
        );

        for bad in [
            vec!["agent", "fs_write"],
            vec!["quatsch", "fs_write", "/x"],
            vec!["agent", "quatsch", "/x"],
            vec!["agent", "fs_write", "/x", "--unbekannt"],
        ] {
            assert!(parse_allow_args(&a(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn deny_net_appears_in_the_ban_list_and_silences_the_host_filter_note() {
        let f = PolicyFile::parse("[agent]\nnet = [\"api.example.com\"]\n[deny]\nnet = true\n")
            .unwrap();
        let set = PolicySet::merge(
            vec![(sepp_policy::Source::File(PathBuf::from("/p")), f)],
            &BuiltinDefaults {
                extra_deny: Vec::new(),
                extra_deny_write: Vec::new(),
                default_mode: Mode::Ask,
            },
            None,
            &ctx(),
        );
        let out = render_policy_table(&set, &caps(true), &[], "/proj", true);
        assert!(out.contains("net  [jeder Akteur, jede Quelle]"), "{out}");
        // Ein Hinweis auf den fehlenden Host-Filter wäre unter einem Vollverbot gegenstandslos.
        assert!(!out.contains("Host-Filter für net-Listen"), "{out}");
    }

    /// Für Plugins gilt die Host-Liste exakt (`host_http`), für Agent und MCP noch nicht.
    #[test]
    fn plugin_host_lists_are_enforced_and_do_not_trigger_the_host_filter_note() {
        let builtin = BuiltinDefaults {
            extra_deny: Vec::new(),
            extra_deny_write: Vec::new(),
            default_mode: Mode::Ask,
        };
        let plugin_only = PolicyFile::parse(
            "[plugin.datev]\nnet = [\"api.datev.de\"]\nenv = [\"DATEV_TOKEN\"]\nfs_read = [\"./\"]\n",
        )
        .unwrap();
        let set = PolicySet::merge(
            vec![(sepp_policy::Source::File(PathBuf::from("/p")), plugin_only)],
            &builtin,
            None,
            &ctx(),
        );
        let out = render_policy_table(&set, &caps(true), &[], "/proj", true);
        assert!(!out.contains("Host-Filter für net-Listen"), "{out}");
        assert!(
            out.contains("Host-Allowlist je Anfrage (host_http)"),
            "{out}"
        );
        assert!(
            out.contains("Secret-Broker ($NAME in host_http-Headern"),
            "{out}"
        );
        assert!(
            out.contains("Pfadprüfung je Aufruf (host_fs_read)"),
            "{out}"
        );
        assert!(!out.contains("Stub"), "{out}");

        let agent = PolicyFile::parse("[agent]\nnet = [\"api.example.com\"]\n").unwrap();
        let set = PolicySet::merge(
            vec![(sepp_policy::Source::File(PathBuf::from("/p")), agent)],
            &builtin,
            None,
            &ctx(),
        );
        let out = render_policy_table(&set, &caps(true), &[], "/proj", true);
        assert!(
            out.contains("Host-Filter für net-Listen bei agent/mcp"),
            "{out}"
        );
    }

    #[test]
    fn section_label_matches_toml_sections() {
        assert_eq!(section_label(&Actor::Agent), "agent");
        assert_eq!(section_label(&Actor::Mcp("git".into())), "mcp.git");
        assert_eq!(section_label(&Actor::Plugin("st".into())), "plugin.st");
    }

    #[test]
    fn select_preset_detects_project_type() {
        let e = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            select_preset(&e(&["Cargo.toml", "src"])),
            Some(Preset::Rust)
        );
        assert_eq!(select_preset(&e(&["package.json"])), Some(Preset::Node));
        assert_eq!(select_preset(&e(&["pyproject.toml"])), Some(Preset::Python));
        assert_eq!(
            select_preset(&e(&["requirements.txt"])),
            Some(Preset::Python)
        );
        // Rust hat Vorrang.
        assert_eq!(
            select_preset(&e(&["package.json", "Cargo.toml"])),
            Some(Preset::Rust)
        );
        assert_eq!(select_preset(&e(&["README.md"])), None);
    }

    #[test]
    fn policy_template_parses_with_and_without_preset() {
        let plain = PolicyFile::parse(&policy_template(None)).unwrap();
        assert_eq!(plain, PolicyFile::default());
        let rust = PolicyFile::parse(&policy_template(Some(Preset::Rust))).unwrap();
        let agent = rust.agent.unwrap();
        assert_eq!(agent.grants.net, NetGrant::All);
        assert!(agent.grants.fs_read.iter().any(|p| p == "~/.cargo"));
        for p in [Preset::Node, Preset::Python] {
            assert!(PolicyFile::parse(&policy_template(Some(p))).is_ok());
        }
    }
}
