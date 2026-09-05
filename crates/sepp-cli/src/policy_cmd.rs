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

/// `sepp policy [show | allow <akteur> <recht> <wert>]`.
pub fn parse_policy_args(args: &[String]) -> Result<PolicyCmd, String> {
    match args.first().map(String::as_str) {
        None | Some("show") => Ok(PolicyCmd::Show),
        Some("allow") => Ok(PolicyCmd::Allow(args[1..].to_vec())),
        Some(other) => Err(format!(
            "policy: unbekannter Unterbefehl '{other}' (erlaubt: show, allow)"
        )),
    }
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
}

pub fn run_policy(cmd: PolicyCmd) -> ExitCode {
    match cmd {
        PolicyCmd::Allow(args) => {
            println!(
                "{}",
                render_allow_hint(&args, session::project_policy_path().ok().as_deref())
            );
            ExitCode::SUCCESS
        }
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
    let trusted = session::is_project_trusted().unwrap_or(false);
    let sources = session::policy_paths(trusted)?;
    let defaults = BuiltinDefaults {
        extra_deny: session::builtin_deny_roots()?,
        default_mode: Mode::Ask,
    };
    let mode_override = std::env::var("SEPP_MODE")
        .ok()
        .and_then(|v| v.parse::<Mode>().ok());
    let set = load_policy_set(&sources, &defaults, mode_override, &ResolveCtx::from_env())?;
    let caps = kernel_capabilities();

    let mut rows: Vec<ActorRow> = Vec::new();
    for cfg in sepp_mcp::load_settings(&session::settings_paths(trusted)?)? {
        let transport = if cfg.transport == "http" {
            Transport::Http
        } else {
            Transport::Stdio
        };
        let legacy = sepp_mcp::policy_from_config(&cfg);
        rows.push(ActorRow {
            actor: Actor::Mcp(cfg.name),
            transport: Some(transport),
            legacy: (!legacy.granted.is_empty()).then_some(legacy),
        });
    }
    for dir in session::plugin_dirs(trusted)? {
        for name in plugin_names(&dir) {
            rows.push(ActorRow {
                actor: Actor::Plugin(name),
                transport: None,
                legacy: None,
            });
        }
    }
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    Ok(render_policy_table(&set, &caps, &rows, &cwd, trusted))
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
) -> String {
    let fs = fs_enforcer(caps);
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
            Some(Transport::Http) => "KEINE (remote)".into(),
            _ => match cap {
                Capability::FsRead { .. } | Capability::FsWrite { .. } => fs.into(),
                Capability::Net { .. } => "TCP gesamt erlaubt (Host-Filter folgt)".into(),
                Capability::Env { .. } => "env_clear + Allowlist".into(),
                Capability::Exec { .. } => format!("Execute-Allowlist ({fs})"),
            },
        },
        Actor::Plugin(_) => match cap {
            Capability::FsRead { .. } => "wasmi-Linker-Gate (host_fs_read: Stub)".into(),
            Capability::Net { .. } => "wasmi-Linker-Gate (host_http: Stub)".into(),
            _ => "wasmi-Linker-Gate (kein Host-Import)".into(),
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

    let mut rows: Vec<[String; 5]> = Vec::new();
    for actor in &actors {
        let row_info = extra.iter().find(|r| r.actor == *actor);
        let transport = row_info.and_then(|r| r.transport);
        let label = actor.to_string();
        let mut has_net = false;
        let mut has_exec = false;
        let mut any = false;
        for e in set.entries.iter().filter(|e| e.actor == *actor) {
            any = true;
            has_net |= matches!(e.cap, Capability::Net { .. });
            has_exec |= matches!(e.cap, Capability::Exec { .. });
            push_row(
                &mut rows,
                [
                    label.clone(),
                    cap_kind(&e.cap).into(),
                    e.raw.clone(),
                    short_source(&e.source),
                    enforcer(actor, &e.cap, caps, transport),
                ],
            );
        }
        if let Some(legacy) = row_info.and_then(|r| r.legacy.as_ref()) {
            for c in &legacy.granted {
                any = true;
                has_net |= matches!(c, Capability::Net { .. });
                has_exec |= matches!(c, Capability::Exec { .. });
                rows.push([
                    label.clone(),
                    cap_kind(c).into(),
                    cap_value(c),
                    "settings.toml (capabilities)".into(),
                    enforcer(actor, c, caps, transport),
                ]);
            }
        }
        match actor {
            Actor::Plugin(_) => {
                if !any {
                    rows.push([
                        label.clone(),
                        "(Manifest)".into(),
                        "Gewährung = Manifest-Anfrage".into(),
                        "–".into(),
                        "wasmi-Linker-Gate".into(),
                    ]);
                } else if !has_net {
                    rows.push([
                        label.clone(),
                        "net".into(),
                        "aus".into(),
                        "eingebaut".into(),
                        "wasmi-Linker-Gate (host_http nicht registriert)".into(),
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
    for actor in &actors {
        for o in set.deny_overlaps(actor) {
            unenforceable.push(format!(
                "Verbot {} unter Gewährung {} ({}): für Kindprozesse nicht durchsetzbar, In-Process-Prüfung greift",
                o.deny.display(),
                o.grant.display(),
                actor
            ));
        }
    }
    if set
        .entries
        .iter()
        .any(|e| matches!(&e.cap, Capability::Net { host } if host != "*"))
    {
        unenforceable
            .push("Host-Filter für net-Listen (TCP gesamt erlaubt; Egress-Proxy folgt)".into());
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

/// Hinweis-Stub für `sepp policy allow <akteur> <recht> <wert>`: nennt Datei und TOML-Schnipsel,
/// bis der Befehl selbst schreibt (Phase 2).
pub fn render_allow_hint(args: &[String], project_policy: Option<&Path>) -> String {
    let usage = "Verwendung: sepp policy allow <agent | mcp.<name> | plugin.<name>> <fs_read | fs_write | net | env | exec> <wert>";
    let (Some(actor), Some(right), Some(value)) = (args.first(), args.get(1), args.get(2)) else {
        return usage.to_string();
    };
    if !matches!(
        right.as_str(),
        "fs_read" | "fs_write" | "net" | "env" | "exec"
    ) {
        return format!("Unbekanntes Recht '{right}'.\n{usage}");
    }
    let section = if actor == "agent" || actor.starts_with("mcp.") || actor.starts_with("plugin.") {
        actor.clone()
    } else {
        return format!("Unbekannter Akteur '{actor}'.\n{usage}");
    };
    let line = if right == "net" && (value == "true" || value == "false") {
        format!("net = {value}")
    } else {
        format!("{right} = [{value:?}]")
    };
    let file = project_policy
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".sepp/policy.toml".into());
    format!(
        "`sepp policy allow` schreibt in einer späteren Version selbst in die Policy-Datei. Bis dahin von Hand:\n\n  \
         Datei: {file}   (global: ~/.sepp/policy.toml)\n\n  [{section}]\n  {line}\n\n\
         Einträge erweitern die eingebauten Defaults; die Datei lädt beim nächsten Start \
         (projektlokal nur nach Trust). Kontrolle: sepp policy"
    )
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
# [mcp.git]                     # ergänzt [mcp.servers.capabilities] aus settings.toml
# fs_write = ["./"]
# exec     = ["git"]
#
# [plugin.string-tools]         # Gewährung für ein WASM-Plugin (effektiv: Schnitt mit dem Manifest)
# net      = ["api.example.com"]
#
# [deny]                        # gewinnt immer; fs_read sperrt Lesen+Schreiben, fs_write nur Schreiben
# fs_read  = ["~/.config/secrets"]
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
            },
            ActorRow {
                actor: Actor::Mcp("fpv7".into()),
                transport: Some(Transport::Http),
                legacy: None,
            },
            ActorRow {
                actor: Actor::Plugin("string-tools".into()),
                transport: None,
                legacy: None,
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
        assert!(out.contains("(Manifest)"), "{out}");
        assert!(out.contains("Execute-Allowlist"), "{out}");
        assert!(out.contains("settings.toml (capabilities)"), "{out}");
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
    fn render_allow_hint_builds_toml_snippet() {
        let hint = render_allow_hint(
            &["agent".into(), "fs_write".into(), "/x".into()],
            Some(Path::new("/proj/.sepp/policy.toml")),
        );
        assert!(hint.contains("[agent]"));
        assert!(hint.contains("fs_write = [\"/x\"]"));
        assert!(hint.contains("/proj/.sepp/policy.toml"));
        let net = render_allow_hint(&["mcp.git".into(), "net".into(), "true".into()], None);
        assert!(net.contains("[mcp.git]"));
        assert!(net.contains("net = true"));
        assert!(render_allow_hint(&[], None).starts_with("Verwendung"));
        assert!(
            render_allow_hint(&["x".into(), "fs_read".into(), "/y".into()], None)
                .contains("Unbekannter Akteur")
        );
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
