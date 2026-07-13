//! `sepp` — CLI-Frontend.
//!
//! Phase 1: One-shot (`sepp -p "<prompt>"`). Phase 2: interaktive TUI (Default, ohne `-p`),
//! persistente Baum-Sessions (`-c`/`-r`) und Compaction.
//!
//! Wichtig: im One-shot-Modus gehen **alle Logs nach STDERR**; stdout ist der reine
//! Daten-/Text-Kanal. Im TUI-Modus wird Tracing nicht initialisiert (sonst würde stderr die
//! Oberfläche zerstören).

mod session;
mod tui;

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use sepp_agent::resources::ResourceSet;
use sepp_agent::{AgentEvent, AgentSession, SubAgentTool};
use sepp_core::{Model, SeppError, ThinkingLevel};
use sepp_hooks::{HookHost, RhaiHookHost};
use sepp_provider::openai::{MLX_BASE_URL, MLX_HOST_PORT};
use sepp_provider::{
    models, AnthropicProvider, OpenAiDialect, OpenAiProvider, Provider, ZaiProvider,
};
use sepp_tools::{builtin_tools, Tool};

use crate::session::SessionSelect;

const SYSTEM_PROMPT: &str = "Du bist sepp mini, ein präziser Coding-/Task-Agent. \
Dir stehen die Tools read, write, edit und bash zur Verfügung; nutze sie, um Aufgaben im \
aktuellen Arbeitsverzeichnis zu lösen. Arbeite in kleinen, überprüfbaren Schritten und \
antworte knapp.";

enum Cmd {
    Version,
    Help,
    /// `sepp init [--global|--system]` — legt das Konfig-Skelett an: projektlokal `<cwd>/.sepp`
    /// (Default, + auto-trust), `--global` in `~/.sepp` (bzw. `$SEPP_HOME`), `--system` als
    /// FHS-Layout (`/etc/sepp` config + `/var/lib/sepp` state).
    Init {
        scope: session::InitScope,
    },
    /// `sepp uninstall [--purge]` — entfernt die Binary (mit `--purge` zusätzlich config- und
    /// state-Root sowie alle projektlokalen `.sepp` aus der Trust-Registry).
    Uninstall {
        purge: bool,
    },
    Run(RunOpts),
}

struct RunOpts {
    /// `Some` → One-shot; `None` → interaktive TUI (außer `rpc`).
    prompt: Option<String>,
    model: Option<String>,
    max_tokens: Option<u64>,
    session: SessionSelect,
    /// `anthropic` (Default) | `openai` | `local` | `zai` | `mlx`.
    provider: Option<String>,
    /// JSONL-RPC über stdin/stdout statt TUI/One-shot.
    rpc: bool,
    /// SQLite-Session-Backend statt JSONL (nur `-p`/`--rpc`; braucht Feature `sqlite`).
    sqlite: bool,
    /// `--think`/`--no-think`: `Some(true/false)` erzwingt Reasoning an/aus; `None` = Default
    /// (z.ai an, sonst aus). Vorrang vor `SEPP_THINK`.
    think: Option<bool>,
    /// `--hide-thinking`: Reasoning nicht anzeigen (Default: gedimmt sichtbar).
    hide_thinking: bool,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args) {
        Ok(Cmd::Version) => {
            println!("sepp {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Cmd::Help) => {
            print_help();
            ExitCode::SUCCESS
        }
        Ok(Cmd::Init { scope }) => run_init(scope),
        Ok(Cmd::Uninstall { purge }) => run_uninstall(purge),
        Ok(Cmd::Run(opts)) => run(opts),
        Err(e) => {
            eprintln!("Fehler: {e}\n");
            print_help();
            ExitCode::from(2)
        }
    }
}

fn parse(args: &[String]) -> Result<Cmd, String> {
    // Subcommands werden nur als **erstes** Positions-Token erkannt, damit Bare-Prompts wie
    // `sepp -p "init …"` unverändert bleiben und nicht im Prompt-Fallback unten landen.
    match args.first().map(String::as_str) {
        Some("init") => {
            let mut scope = session::InitScope::Project;
            for a in &args[1..] {
                match a.as_str() {
                    "--global" | "-g" => scope = session::InitScope::Global,
                    "--system" => scope = session::InitScope::System,
                    // expliziter Default — fürs Skripten/die Klarheit erlaubt.
                    "--here" | "--local" => scope = session::InitScope::Project,
                    other => return Err(format!("init: unbekannte Option: {other}")),
                }
            }
            return Ok(Cmd::Init { scope });
        }
        Some("uninstall") => {
            let mut purge = false;
            for a in &args[1..] {
                match a.as_str() {
                    "--purge" => purge = true,
                    other => return Err(format!("uninstall: unbekannte Option: {other}")),
                }
            }
            return Ok(Cmd::Uninstall { purge });
        }
        _ => {}
    }

    let mut prompt: Option<String> = None;
    let mut model: Option<String> = None;
    let mut max_tokens: Option<u64> = None;
    let mut select = SessionSelect::New;
    let mut provider: Option<String> = None;
    let mut rpc = false;
    let mut sqlite = false;
    let mut think: Option<bool> = None;
    let mut hide_thinking = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => return Ok(Cmd::Help),
            "-V" | "--version" => return Ok(Cmd::Version),
            "--rpc" => rpc = true,
            "--sqlite" => sqlite = true,
            "--think" => think = Some(true),
            "--no-think" => think = Some(false),
            "--hide-thinking" => hide_thinking = true,
            "--provider" => {
                i += 1;
                provider = Some(
                    args.get(i)
                        .ok_or("--provider braucht ein Argument")?
                        .clone(),
                );
            }
            "-p" | "--print" => {
                i += 1;
                prompt = Some(
                    args.get(i)
                        .ok_or("-p/--print braucht ein Argument")?
                        .clone(),
                );
            }
            "-m" | "--model" => {
                i += 1;
                model = Some(
                    args.get(i)
                        .ok_or("-m/--model braucht ein Argument")?
                        .clone(),
                );
            }
            "--max-tokens" => {
                i += 1;
                let v = args.get(i).ok_or("--max-tokens braucht ein Argument")?;
                max_tokens = Some(v.parse().map_err(|_| "ungültiger --max-tokens-Wert")?);
            }
            "-c" | "--continue" => select = SessionSelect::Continue,
            "-r" | "--resume" => {
                // Optionales ID-Argument, wenn der nächste Token keine Option ist.
                match args.get(i + 1) {
                    Some(next) if !next.starts_with('-') => {
                        i += 1;
                        select = SessionSelect::Resume(Some(next.clone()));
                    }
                    _ => select = SessionSelect::Resume(None),
                }
            }
            other if other.starts_with('-') => return Err(format!("unbekannte Option: {other}")),
            other => {
                if prompt.is_some() {
                    return Err("mehrere Prompts angegeben".into());
                }
                prompt = Some(other.to_string());
            }
        }
        i += 1;
    }

    Ok(Cmd::Run(RunOpts {
        prompt,
        model,
        max_tokens,
        session: select,
        provider,
        rpc,
        sqlite,
        think,
        hide_thinking,
    }))
}

/// `SEPP_THINK`-Wert → optionaler Bool (Unbekanntes ⇒ `None`, damit der Default greift).
fn parse_think_env(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// CLI/Env → effektive Reasoning-Stufe. Default-AN ist **z.ai-spezifisch**; andere Provider
/// bleiben Off, sofern nicht explizit `--think`/`SEPP_THINK` gesetzt wird. `--think`/`--no-think`
/// haben Vorrang vor `SEPP_THINK` (wie `--provider` vor `SEPP_PROVIDER`). „An" = `Medium` (4096),
/// nicht `High`: dieselbe Stufe fließt zu Anthropic, das `budget_tokens < max_tokens` verlangt —
/// bei Default-`max_tokens=8192` wäre `High=8192` grenzwertig, `Medium` ist sicher. z.ai ignoriert
/// das Budget (binär an/aus).
fn resolve_thinking(flag: Option<bool>, env: Option<&str>, is_zai: bool) -> ThinkingLevel {
    match flag.or_else(|| env.and_then(parse_think_env)) {
        Some(true) => ThinkingLevel::Medium,
        Some(false) => ThinkingLevel::Off,
        None => {
            if is_zai {
                ThinkingLevel::Medium
            } else {
                ThinkingLevel::Off
            }
        }
    }
}

/// Env-Wert mit der Provider-Semantik „leer/Whitespace = nicht gesetzt" (getrimmt) — exakt
/// die Auflösung, die auch `openai::from_env`/`mlx_config` nutzen. Eine Quelle, kein Drift:
/// Frühchecks und Provider dürfen „gesetzt?" nie unterschiedlich beantworten (sonst geht z. B.
/// ein Request ohne Frühwarnung an api.openai.com).
fn env_nonempty(name: &str) -> Option<String> {
    sepp_provider::openai::nonempty_trimmed(std::env::var(name).ok())
}

/// Frühcheck für die OpenAI-Dialekt-Provider (pur, testbar ohne Env-Mutation). Liefert das
/// `reason`-Tag für den Audit-Eintrag oder `None`, wenn der Start zulässig ist:
/// - `openai` ohne base_url-Override UND ohne Key → `missing_api_key` (der Request ginge an
///   api.openai.com und endete als roher 401).
/// - `local` ohne base_url → `missing_base_url`: local MEINT einen lokalen Endpunkt; der
///   from_env-Fallback auf api.openai.com wäre ein stiller Cloud-Egress samt Key und Prompt.
fn openai_local_precheck(
    provider: &str,
    base: Option<&str>,
    key: Option<&str>,
) -> Option<&'static str> {
    match provider {
        "openai" if base.is_none() && key.is_none() => Some("missing_api_key"),
        "local" if base.is_none() => Some("missing_base_url"),
        _ => None,
    }
}

fn print_help() {
    eprintln!(
        "sepp mini — leichtgewichtiger Agent\n\n\
         Verwendung:\n\
         \x20 sepp                      Interaktive TUI (neue Session)\n\
         \x20 sepp -c                   TUI, jüngste Session fortsetzen\n\
         \x20 sepp -p \"<prompt>\"        Einen Prompt nicht-interaktiv ausführen\n\
         \x20 sepp init                 Konfig-Skelett in ./.sepp anlegen (+ Projekt vertrauen)\n\
         \x20 sepp init --global        stattdessen in ~/.sepp (bzw. $SEPP_HOME)\n\
         \x20 sepp init --system        FHS-Layout: /etc/sepp (config) + /var/lib/sepp (state)\n\
         \x20 sepp uninstall [--purge]  Binary entfernen (mit --purge auch config+state-Root + projektlokale .sepp)\n\n\
         Optionen:\n\
         \x20 -p, --print <text>        One-shot-Prompt (sonst startet die TUI)\n\
         \x20 -c, --continue            Jüngste Session des Projekts fortsetzen\n\
         \x20 -r, --resume [id]         Session per ID-Präfix wählen (ohne id: jüngste)\n\
         \x20 -m, --model <id>          Modell-ID (Default: {default})\n\
         \x20     --max-tokens <n>      Max. Output-Tokens (Default: 8192)\n\
         \x20     --provider <name>     anthropic (Default) | openai | local | zai | mlx\n\
         \x20                           (ohne Angabe aus -m abgeleitet, z. B. glm-* → zai)\n\
         \x20     --think / --no-think  Reasoning erzwingen/abschalten (z.ai: Default an)\n\
         \x20     --hide-thinking       Reasoning nicht anzeigen (Default: gedimmt sichtbar)\n\
         \x20     --rpc                 JSONL-RPC über stdin/stdout (statt TUI/One-shot)\n\
         \x20     --sqlite              SQLite-Session-Backend (nur -p/--rpc; Feature 'sqlite')\n\
         \x20 -h, --help                Diese Hilfe\n\
         \x20 -V, --version             Version\n\n\
         TUI-Befehle: /new /resume /tree /compact /model [id] /trust /reload /hide /show /quit\n\
         \x20            (plus /<name> für Prompt-Templates aus ~/.sepp/prompts)\n\n\
         Umgebung:\n\
         \x20 ANTHROPIC_API_KEY         Pflicht für Anthropic-Live-Aufrufe\n\
         \x20 OPENAI_API_KEY            OpenAI (optional bei lokalen Servern; --provider mlx\n\
         \x20                           sendet ihn nur bei explizit gesetztem OPENAI_BASE_URL)\n\
         \x20 OPENAI_BASE_URL           OpenAI-kompatible base_url (Ollama/vLLM/local/mlx)\n\
         \x20                           (Pflicht für --provider local; --provider mlx:\n\
         \x20                           Default http://localhost:1234/v1 = LM Studio)\n\
         \x20 ZAI_API_KEY               z.ai/Zhipu-GLM (Pflicht für --provider zai)\n\
         \x20 ZAI_BASE_URL              z.ai base_url überschreiben (Default api.z.ai)\n\
         \x20 SEPP_HOME                 globale Konfig-Wurzel verlegen (Default ~/.sepp)\n\
         \x20 SEPP_PROVIDER             Default-Provider, wenn --provider fehlt\n\
         \x20 SEPP_THINK                Default-Reasoning (on/off), wenn --think/--no-think fehlt\n\
         \x20 RUST_LOG                  Log-Level (One-shot/RPC; Logs nach stderr)",
        default = models::DEFAULT_MODEL_ID
    );
}

/// Vorlage für eine frische `~/.sepp/settings.toml` — **komplett auskommentiert** und damit gültig
/// (parst zu „keine Server"). Zeigt je einen `stdio`- und `http`-MCP-Server samt Capabilities.
const SETTINGS_TEMPLATE: &str = r#"# sepp mini — globale Einstellungen (~/.sepp/settings.toml)
#
# Hier werden MCP-Server als Tool-Quellen deklariert. Jeder Server braucht capabilities
# (Default DENY). Entferne die Kommentarzeichen und passe an. Doppelte `name` sind ein Fehler;
# eine leere/komplett auskommentierte Datei ist gültig.
#
# Beispiel: stdio-Server (lokaler Subprozess)
# [[mcp.servers]]
# name = "git"
# transport = "stdio"
# command = ["uvx", "mcp-server-git"]
# [mcp.servers.capabilities]
# fs_read  = ["./"]
# fs_write = ["./"]
# exec     = ["git"]
#
# Beispiel: http-Server (entfernter Endpunkt)
# [[mcp.servers]]
# name = "example"
# transport = "http"
# url = "https://mcp.example.com"
# [mcp.servers.capabilities]
# net = ["mcp.example.com"]
"#;

/// `sepp init [--global|--system]` — legt das Konfig-Skelett samt kommentierter Beispiel-
/// `settings.toml` an (idempotent). Default ist projektlokal `<cwd>/.sepp` (nur Config, danach
/// auto-trust); `--global` zielt auf `~/.sepp` bzw. `$SEPP_HOME`; `--system` legt das FHS-Layout an
/// (`/etc/sepp` config + `/var/lib/sepp` state, via `$SEPP_CONFIG_DIR`/`$SEPP_STATE_DIR` verlegbar).
/// Sessions/Trust liegen zentral im state_root, daher legt der State-Teil `sessions/` an. Läuft ohne
/// Tokio/Provider.
fn run_init(scope: session::InitScope) -> ExitCode {
    let (config, state) = match session::init_roots(scope) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Fehler: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = init_config_at(&config) {
        eprintln!("Fehler: {e}");
        return ExitCode::FAILURE;
    }
    if let Some(state) = &state {
        if let Err(e) = init_state_at(state, scope == session::InitScope::System) {
            eprintln!("Fehler: {e}");
            return ExitCode::FAILURE;
        }
    }
    // Projektlokale Erweiterungen werden nur nach Trust geladen — sonst legt `init` etwas an, das
    // nie greift. Daher cwd direkt vertrauen; ein Fehler ist nicht fatal (manuell via `/trust`).
    let mut trusted = false;
    if scope == session::InitScope::Project {
        match session::trust_current_project() {
            Ok(()) => trusted = true,
            Err(e) => {
                eprintln!("Warnung: Projekt konnte nicht automatisch vertraut werden: {e}");
                eprintln!("In der TUI nachholen mit: /trust");
            }
        }
    }
    println!(
        "sepp init abgeschlossen: {}{}",
        config.display(),
        if trusted { " (vertraut)" } else { "" }
    );
    if let Some(state) = &state {
        println!("  state: {}", state.display());
    }
    if scope == session::InitScope::System {
        println!();
        println!("Für eindeutige Laufzeit-Auflösung in die Shell-Umgebung aufnehmen");
        println!("(z. B. /etc/profile.d/sepp.sh) — optional, da ein vorhandenes System-Setup auch");
        println!("ohne Env gefunden wird:");
        println!("  export SEPP_CONFIG_DIR={}", config.display());
        if let Some(state) = &state {
            println!("  export SEPP_STATE_DIR={}", state.display());
        }
        println!("Binary systemweit: SEPP_BIN_DIR=/usr/local/bin sh install.sh");
    }
    ExitCode::SUCCESS
}

/// Erzeugt das **Config**-Skelett (`skills/`, `prompts/`, `hooks/`, `plugins/`) und eine kommentierte
/// `settings.toml` unterhalb `root`; vorhandene Pfade bleiben unverändert. Die Subdir-Namen müssen
/// **exakt** den Lese-Literalen in `session.rs` entsprechen, sonst wird das Angelegte nie gelesen.
fn init_config_at(root: &Path) -> anyhow::Result<()> {
    ensure_dir(root)?;
    for sub in ["skills", "prompts", "hooks", "plugins"] {
        ensure_dir(&root.join(sub))?;
    }
    let settings = root.join("settings.toml");
    if settings.exists() {
        println!("übersprungen (existiert): {}", settings.display());
    } else {
        std::fs::write(&settings, SETTINGS_TEMPLATE)?;
        println!("angelegt: {}", settings.display());
    }
    Ok(())
}

/// Legt die **State**-Wurzel an (`sessions/`). Bei `restrictive` (System-Installation) wird der
/// State-Root auf `0700` gesetzt — hier landen künftig Trust und `auth.json`.
fn init_state_at(root: &Path, restrictive: bool) -> anyhow::Result<()> {
    ensure_dir(root)?;
    ensure_dir(&root.join("sessions"))?;
    #[cfg(unix)]
    if restrictive {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = restrictive;
    Ok(())
}

/// Legt ein Verzeichnis an, falls es noch nicht existiert, und meldet „angelegt"/„übersprungen".
fn ensure_dir(p: &Path) -> anyhow::Result<()> {
    if p.is_dir() {
        println!("übersprungen (existiert): {}", p.display());
    } else {
        std::fs::create_dir_all(p)?;
        println!("angelegt: {}", p.display());
    }
    Ok(())
}

/// `sepp uninstall [--purge]` — entfernt die laufende Binary (Unix: Selbstlöschung ist erlaubt,
/// der Inode bleibt bis Prozessende). Mit `--purge` zusätzlich **beide** globalen Wurzeln
/// (config_root und state_root, z. B. `/etc/sepp` und `/var/lib/sepp`) **und** alle projektlokalen
/// `.sepp` aus der Trust-Registry.
fn run_uninstall(purge: bool) -> ExitCode {
    match uninstall(purge) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Fehler: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Bestimmt die Verzeichnisse, die `--purge` entfernt: jedes projektlokale `<trusted>/.sepp` (Anker
/// via Trust-Registry) plus `<cwd>/.sepp`, gefolgt von den beiden globalen Wurzeln (config_root +
/// state_root). Dedupliziert (config==state bei `~/.sepp`-Default ⇒ nur einmal); die globalen Roots
/// stehen **am Ende** (sauberes Reporting und damit `trust.json` darin erst nach dem Auslesen
/// entfernt wird). Pure Funktion — ohne Env/FS testbar.
fn purge_targets(
    config_root: &Path,
    state_root: &Path,
    trusted: &[PathBuf],
    cwd: Option<&Path>,
) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = trusted.iter().map(|p| p.join(".sepp")).collect();
    if let Some(cwd) = cwd {
        candidates.push(cwd.join(".sepp"));
    }
    let globals = [config_root.to_path_buf(), state_root.to_path_buf()];
    let mut out: Vec<PathBuf> = Vec::new();
    for c in candidates {
        // Eine globale Wurzel NICHT als Projekt-Ziel doppeln (Fall: `init` aus dem Home).
        if !globals.contains(&c) && !out.contains(&c) {
            out.push(c);
        }
    }
    for g in globals {
        if !out.contains(&g) {
            out.push(g);
        }
    }
    out
}

fn uninstall(purge: bool) -> anyhow::Result<()> {
    // Hinweis: Unter `cargo run` zeigt current_exe() auf die Dev-Binary in target/ — die würde dann
    // entfernt. Für den Distributions-Fall (~/.local/bin/sepp) ist genau das gewollt.
    let exe = std::env::current_exe()?;
    std::fs::remove_file(&exe)?;
    println!("Entfernt: {}", exe.display());

    // Ziele VOR dem Löschen bestimmen: `trust.json` liegt im state_root und muss vorher gelesen
    // werden (deshalb stehen die globalen Roots in `purge_targets` zuletzt). cwd kanonisieren, damit
    // es sauber gegen die kanonischen Trust-Keys dedupliziert.
    let config = session::config_root()?;
    let state = session::state_root()?;
    let trusted = session::trusted_projects().unwrap_or_default();
    let cwd = std::env::current_dir()
        .ok()
        .map(|c| std::fs::canonicalize(&c).unwrap_or(c));
    let targets = purge_targets(&config, &state, &trusted, cwd.as_deref());

    if purge {
        for target in &targets {
            if target.is_dir() {
                // Fehler je Ziel tolerieren — ein nicht löschbares Verzeichnis bricht nicht alles ab.
                match std::fs::remove_dir_all(target) {
                    Ok(()) => println!("Entfernt (--purge): {}", target.display()),
                    Err(e) => eprintln!("Konnte {} nicht entfernen: {e}", target.display()),
                }
            } else {
                println!("Nicht gefunden (übersprungen): {}", target.display());
            }
        }
    } else {
        let existing: Vec<&PathBuf> = targets.iter().filter(|t| t.is_dir()).collect();
        if !existing.is_empty() {
            println!("Hinweis: folgende Nutzerdaten bleiben erhalten:");
            for t in existing {
                println!("         {}", t.display());
            }
            println!("         Zum vollständigen Entfernen: sepp uninstall --purge");
        }
    }
    println!("Deinstallation abgeschlossen.");
    Ok(())
}

fn run(opts: RunOpts) -> ExitCode {
    // current_thread genügt (I/O-gebunden); spart Worker-Thread-Churn beim Start.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Fehler: Tokio-Runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match rt.block_on(run_async(opts)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if let Some(SeppError::Aborted) = e.downcast_ref::<SeppError>() {
                eprintln!("\n[abgebrochen]");
                return ExitCode::from(130);
            }
            eprintln!("\nFehler: {e}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

async fn run_async(opts: RunOpts) -> anyhow::Result<()> {
    // Flag-Constraints früh prüfen, damit der Hinweis nicht vom Provider-Key-Fehler verdeckt wird.
    if opts.sqlite && opts.prompt.is_none() && !opts.rpc {
        anyhow::bail!("--sqlite ist nur mit -p/--rpc nutzbar (die TUI nutzt JSONL)");
    }

    // Provider-Auflösung: explizit (--provider > SEPP_PROVIDER) schlägt alles. Fehlt das, wird er
    // aus dem gewählten Modell abgeleitet — `--model glm-5.2` landet so automatisch bei `zai` statt
    // am OpenAI-Endpunkt (eine Hauptquelle der „falscher Endpunkt"-Fehler). Erst danach der
    // Default `anthropic`.
    let provider_kind = opts
        .provider
        .clone()
        .or_else(|| std::env::var("SEPP_PROVIDER").ok())
        .or_else(|| {
            opts.model
                .as_deref()
                .and_then(models::find_model)
                .map(|m| m.provider)
        })
        .unwrap_or_else(|| "anthropic".into());
    let is_openai = matches!(provider_kind.as_str(), "openai" | "local");
    let is_zai = provider_kind == "zai";
    let is_mlx = provider_kind == "mlx";
    // Reasoning-Stufe auflösen: --think/--no-think > SEPP_THINK > Provider-Default (z.ai an, sonst aus).
    let thinking = resolve_thinking(
        opts.think,
        std::env::var("SEPP_THINK").ok().as_deref(),
        is_zai,
    );
    // Start-Hinweise: im TUI-Modus gesammelt und dort im Chatfenster angezeigt — ein eprintln
    // verpufft hinter dem Alternate-Screen; bei -p/--rpc bleibt stderr der sichtbare Kanal.
    let interactive = opts.prompt.is_none() && !opts.rpc;
    let mut startup_notices: Vec<String> = Vec::new();
    let mut startup_notice = |msg: String| {
        if interactive {
            startup_notices.push(msg);
        } else {
            eprintln!("{msg}");
        }
    };
    // --think/SEPP_THINK ist bei openai/mlx wirkungslos (kein Request-seitiges Reasoning-Feld;
    // anthropic/zai haben eins, local steuert Ollamas Server-Default-Thinking binär über
    // `reasoning_effort` — OpenAiDialect::Local, openai.rs) — explizit gewünschtes Reasoning
    // wäre sonst ein stiller No-op.
    if thinking != ThinkingLevel::Off && (provider_kind == "openai" || is_mlx) {
        startup_notice(format!(
            "Hinweis: --think/SEPP_THINK hat bei --provider {provider_kind} keine Wirkung — \
             der Wert wird ignoriert."
        ));
    }
    // Session-Store VOR den Key-Checks bauen, damit jeder Start auditierbar ist: bricht ein
    // Key-Check ab, hängen wir einen `aborted`-Eintrag an und fsyncen — die Datei existiert auch
    // ohne erfolgreichen Provider-Start (Audit-Trail). `build_store` braucht weder Provider noch
    // Modell. `mut`, weil der Abbruch-Pfad in den Store schreibt.
    let mut store = build_store(opts.sqlite, opts.prompt.is_some(), opts.rpc, &opts.session)?;
    // Effektiver OPENAI_BASE_URL-Override — EINE Auflösung für die Frühchecks hier und den
    // mlx-Preflight unten, mit exakt der Semantik der Provider (leer/Whitespace = nicht gesetzt).
    let base_override = env_nonempty("OPENAI_BASE_URL");
    // Frühchecks openai/local: echtes OpenAI braucht einen Key (sonst roher 401 erst im
    // Stream), --provider local einen lokalen Endpunkt (sonst ginge der Request samt
    // OPENAI_API_KEY still an api.openai.com — Cloud-Egress, obwohl „lokal" gemeint war).
    if let Some(reason) = openai_local_precheck(
        &provider_kind,
        base_override.as_deref(),
        env_nonempty("OPENAI_API_KEY").as_deref(),
    ) {
        let msg = if reason == "missing_base_url" {
            "OPENAI_BASE_URL nicht gesetzt — --provider local braucht einen lokalen \
             OpenAI-kompatiblen Endpunkt:\n  \
             export OPENAI_BASE_URL=http://localhost:11434/v1   # Ollama\n  \
             export OPENAI_BASE_URL=http://localhost:8000/v1    # vLLM\n  \
             LM Studio: --provider mlx · echtes OpenAI: --provider openai (mit OPENAI_API_KEY)"
        } else {
            "OPENAI_API_KEY nicht gesetzt — setze den Key, oder nutze --provider local \
             mit OPENAI_BASE_URL für lokale Endpunkte"
        };
        return Err(abort_with_audit(
            store.as_mut(),
            msg,
            serde_json::json!({ "reason": reason, "provider": provider_kind }),
        )
        .await);
    }
    // Anthropic braucht ANTHROPIC_API_KEY — hier früh + hilfreich scheitern statt mit dem nackten
    // "ANTHROPIC_API_KEY nicht gesetzt" aus AnthropicProvider::from_env(). Die Prüfung spiegelt
    // bewusst from_env (anthropic.rs): einzige Quelle ist ANTHROPIC_API_KEY, leer/Whitespace zählt
    // als fehlend. Zieht from_env künftig auch ~/.sepp/auth.json heran, muss dieser Check mit.
    if provider_kind == "anthropic" && env_nonempty("ANTHROPIC_API_KEY").is_none() {
        let msg = "ANTHROPIC_API_KEY nicht gesetzt — eine der Optionen:\n  \
             - Key setzen:     export ANTHROPIC_API_KEY=…\n  \
             - lokales Modell: --provider local  (bzw. OPENAI_BASE_URL für Ollama/vLLM)\n  \
             - OpenAI:         --provider openai  (mit OPENAI_API_KEY)\n\
             Konfiguration liegt unter ~/.sepp — anlegen mit `sepp init`.";
        return Err(abort_with_audit(
            store.as_mut(),
            msg,
            serde_json::json!({ "reason": "missing_api_key", "provider": provider_kind }),
        )
        .await);
    }
    // z.ai (Zhipu/GLM) braucht ZAI_API_KEY — anders als lokale OpenAI-Endpunkte ist der Key
    // Pflicht, daher hier früh + hilfreich scheitern statt erst beim 401.
    if provider_kind == "zai" && env_nonempty("ZAI_API_KEY").is_none() {
        let msg = "ZAI_API_KEY nicht gesetzt — Key auf https://z.ai holen (Format id.secret) und setzen:\n  \
             export ZAI_API_KEY=…\n  \
             (optional ZAI_BASE_URL für einen abweichenden Endpunkt, z. B. die China-Region)";
        return Err(abort_with_audit(
            store.as_mut(),
            msg,
            serde_json::json!({ "reason": "missing_api_key", "provider": provider_kind }),
        )
        .await);
    }
    if is_mlx {
        // Modell muss explizit gewählt werden — sepp schreibt kein Modell vor; LM Studio bedient
        // das jeweils geladene Modell, dessen Identifier der Nutzer mit -m angibt.
        if opts.model.is_none() {
            let msg = format!(
                "Kein Modell angegeben. Wähle mit -m <modell> das in LM Studio geladene Modell\n  \
                 (Identifier siehst du in LM Studio oder via GET {}/models).",
                base_override.as_deref().unwrap_or(MLX_BASE_URL)
            );
            return Err(abort_with_audit(
                store.as_mut(),
                &msg,
                serde_json::json!({ "reason": "missing_model", "provider": provider_kind }),
            )
            .await);
        }
        // Früh + hilfreich scheitern, wenn LM Studios lokaler Server nicht läuft — statt rohem
        // Verbindungsfehler erst im Stream. Nur für den Default-Endpunkt; bei gesetztem
        // OPENAI_BASE_URL vertraut sepp der Nutzerkonfiguration. Async + Hostname statt fixer
        // IPv4-Adresse: getaddrinfo probiert ::1 UND 127.0.0.1 (Muster wie der MCP-Connect
        // weiter unten), und der current_thread-Reaktor bleibt frei (kein blockierender Syscall).
        if base_override.is_none() {
            let up = matches!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(700),
                    tokio::net::TcpStream::connect(MLX_HOST_PORT),
                )
                .await,
                Ok(Ok(_))
            );
            if !up {
                let msg = format!(
                    "Kein lokaler MLX-Server auf http://{MLX_HOST_PORT} erreichbar.\n  \
                     - LM Studio öffnen → Developer → Local Server starten\n  \
                     - dort ein tool-fähiges MLX-Modell (mit Function-/Tool-Calling) laden\n  \
                     - LM Studio noch nicht installiert? https://lmstudio.ai\n  \
                     - abweichender Endpunkt/Port? OPENAI_BASE_URL setzen."
                );
                return Err(abort_with_audit(
                    store.as_mut(),
                    &msg,
                    serde_json::json!({ "reason": "mlx_server_unreachable", "provider": provider_kind }),
                )
                .await);
            }
        }
    }
    let provider: Arc<dyn Provider> = match provider_kind.as_str() {
        "anthropic" => Arc::new(AnthropicProvider::from_env()?),
        "openai" => Arc::new(OpenAiProvider::from_env()?),
        // Local-Dialekt: schaltet Ollamas Server-Default-Thinking ab (reasoning_effort
        // "none" bei Thinking Off) — sonst bleibt stdout nach Tool-Results teils leer.
        "local" => Arc::new(OpenAiProvider::from_env()?.with_dialect(OpenAiDialect::Local)),
        "mlx" => Arc::new(OpenAiProvider::mlx_from_env()?),
        "zai" => Arc::new(ZaiProvider::from_env()?),
        other => anyhow::bail!("unbekannter Provider: {other} (anthropic|openai|local|zai|mlx)"),
    };

    let model = match opts.model {
        Some(id) => match models::find_model(&id) {
            Some(m) => {
                // Registriertes Modell bei einem ABWEICHEND und EXPLIZIT gewählten Provider:
                // warnen, aber durchlassen (der Mensch weiß evtl., was er tut). Ohne explizite
                // Wahl wird der Provider oben aus dem Modell abgeleitet, dann greift das nie. Der
                // früher unterdrückte Fall „GLM-Modell auf --provider local/openai" warnt jetzt
                // bewusst — er sendet GLM an api.openai.com und scheitert dort am 401.
                if m.provider != provider_kind {
                    startup_notice(format!(
                        "Hinweis: Modell '{}' gehört zu Provider '{}', gewählt ist \
                         '{provider_kind}' — die Anfrage geht an dessen Endpunkt und schlägt fehl, \
                         wenn die Endpunkte inkompatibel sind.",
                        m.id, m.provider
                    ));
                }
                m
            }
            None => custom_model(id, &provider_kind),
        },
        // z.ai: aktuelles Flaggschiff als Default.
        None if is_zai => {
            models::find_model("glm-5.2").unwrap_or_else(|| custom_model("glm-5.2".into(), "zai"))
        }
        // OpenAI hat keine Modell-Registry hier → sinnvoller Default.
        None if is_openai => custom_model("gpt-4o-mini".into(), &provider_kind),
        None => models::default_model(),
    };
    let threshold = sepp_agent::default_compact_threshold(&model);
    // `store` wurde bereits vor den Key-Checks gebaut (Audit jeden Start).

    // Tier 0: Resources (Skills → System-Prompt, Prompt-Templates → Slash-Commands).
    let trusted = session::is_project_trusted().unwrap_or(false);
    let resources = ResourceSet::load(&session::resource_roots(trusted)?);
    let system = format!("{SYSTEM_PROMPT}{}", resources.system_prompt_addition());

    // Tier 1: Hooks (Rhai) aus den Hook-Verzeichnissen.
    let hooks: Option<Box<dyn HookHost>> =
        match RhaiHookHost::from_dirs(&session::hook_dirs(trusted)?) {
            Ok(h) if !h.is_empty() => Some(Box::new(h)),
            Ok(_) => None,
            Err(e) => anyhow::bail!("Hooks laden fehlgeschlagen: {e}"),
        };

    // Tier 3: MCP-Server (built-in + MCP in EINEM Toolset; Namens-Präfix bei Kollision).
    // Connects laufen NEBENLÄUFIG (join_all), jeder zeitlich begrenzt — ein hängender Server
    // verzögert so höchstens um ein Timeout, nicht um die Summe aller Timeouts (Cold-Start).
    let mut tools = builtin_tools();
    let mut taken: HashSet<String> = tools.iter().map(|t| t.spec().name).collect();
    let connect_timeout = std::time::Duration::from_secs(20);
    let mcp_configs = sepp_mcp::load_settings(&session::settings_paths(trusted)?)?;
    let mcp_results = futures::future::join_all(mcp_configs.iter().map(|cfg| async move {
        (
            cfg.name.clone(),
            tokio::time::timeout(connect_timeout, sepp_mcp::connect(cfg)).await,
        )
    }))
    .await;
    // Ergebnisse sequenziell auswerten → deterministische Namens-Vergabe in Config-Reihenfolge.
    for (name, res) in mcp_results {
        match res {
            Ok(Ok(conn)) => {
                let n = conn.tool_count();
                tools.append(&mut conn.into_tools(&mut taken));
                eprintln!("MCP '{name}': {n} Tools verbunden");
            }
            Ok(Err(e)) => eprintln!("MCP '{name}' übersprungen: {e}"),
            Err(_) => eprintln!(
                "MCP '{name}' übersprungen: Timeout ({}s) beim Verbinden",
                connect_timeout.as_secs()
            ),
        }
    }

    // Tier 2: WASM-Plugins (capability-gated; Namens-Präfix `wasm__` bei Kollision).
    let wasm_host = sepp_wasm::WasmHost::new();
    let mut n_wasm = 0usize;
    for dir in session::plugin_dirs(trusted)? {
        for mut plugin in wasm_host.discover(&dir) {
            let exposed = sepp_mcp::resolve_name(&taken, "wasm", &plugin.spec().name);
            taken.insert(exposed.clone());
            plugin.rename(exposed);
            tools.push(Arc::new(plugin));
            n_wasm += 1;
        }
    }
    if n_wasm > 0 {
        eprintln!("WASM: {n_wasm} Plugins geladen");
    }

    // Phase 4: nativer Sub-Agent als Tool (`task`) — isolierter Kontext, eigenes (read/write/
    // edit/bash) Toolset, kein eigener `task` (keine Rekursion).
    let sub = SubAgentTool::new(Arc::clone(&provider), model.clone())
        .tools(builtin_tools())
        .max_tokens(opts.max_tokens.unwrap_or(8192))
        .thinking(thinking);
    let sub_name = sepp_mcp::resolve_name(&taken, "agent", &sub.spec().name);
    taken.insert(sub_name.clone());
    tools.push(Arc::new(sub.name(sub_name)));

    let mut builder = AgentSession::builder()
        .provider(Arc::clone(&provider))
        .model(model)
        .system_prompt(system)
        .tools(tools)
        .max_tokens(opts.max_tokens.unwrap_or(8192))
        .thinking(thinking)
        .session(store)
        .auto_compact_threshold(threshold);
    if let Some(h) = hooks {
        builder = builder.hooks(h);
    }
    let mut agent = builder.build()?;

    if opts.rpc {
        init_tracing();
        return run_rpc(&mut agent).await;
    }

    match opts.prompt {
        // One-shot: streamt nach stdout und persistiert die Session.
        Some(text) => {
            init_tracing();
            let cancel = CancellationToken::new();
            let cancel_signal = cancel.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    cancel_signal.cancel();
                }
            });

            // Reasoning gedimmt nach STDERR (Default sichtbar; --hide-thinking unterdrückt es).
            // stdout bleibt strikt der Datenkanal (nur TextDelta) — Invariante des RPC/Pipe-Vertrags.
            let show_thinking = !opts.hide_thinking;
            let on_event = |ev: AgentEvent| match ev {
                AgentEvent::TextDelta(t) => {
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(t.as_bytes());
                    let _ = out.flush();
                }
                AgentEvent::ThinkingDelta(t) if show_thinking => {
                    let mut err = std::io::stderr().lock();
                    let _ = write!(err, "\x1b[2m{t}\x1b[0m");
                    let _ = err.flush();
                }
                AgentEvent::ToolStart { name, .. } => {
                    eprintln!("\x1b[2m· {name} …\x1b[0m");
                }
                AgentEvent::Error(msg) => {
                    eprintln!("\n\x1b[31m[Fehler]\x1b[0m {msg}");
                }
                _ => {}
            };

            // Ergebnis fangen, NICHT sofort `?` — damit Finalize in BEIDEN Armen (Erfolg wie
            // Fehler) läuft und die Session durabel abgeschlossen wird.
            let res = agent.prompt(&text, &on_event, cancel).await;
            println!();
            if let Err(e) = agent.finalize().await {
                eprintln!("Hinweis: Session-Abschluss fehlgeschlagen: {e}");
            }
            res?;
            Ok(())
        }
        // Interaktiv: TUI (kein Tracing → stderr bleibt sauber).
        None => {
            let prompts: Vec<(String, String)> = resources
                .prompts
                .into_iter()
                .map(|p| (p.name, p.content))
                .collect();
            tui::run(
                agent,
                prompts,
                SYSTEM_PROMPT.to_string(),
                !opts.hide_thinking,
                startup_notices,
            )
            .await
        }
    }
}

/// JSONL-RPC: liest pro Zeile einen Request von stdin, streamt Ereignisse als JSONL nach stdout.
/// Request: `{"type":"prompt","text":"…"}`. Antworten: `text`/`tool_start`/`tool_end`/`error`,
/// abgeschlossen mit `{"type":"done"}`. So läuft derselbe Kern hinter beliebigen Frontends.
async fn run_rpc(agent: &mut AgentSession) -> anyhow::Result<()> {
    use tokio::io::AsyncBufReadExt;

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    loop {
        // Ctrl+C im Leerlauf (wartend auf stdin) oder EOF beendet den Server sauber.
        let line = tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            res = lines.next_line() => match res? {
                Some(l) => l,
                None => break,
            },
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                emit_rpc(&serde_json::json!({ "type": "error", "message": format!("json: {e}") }));
                continue;
            }
        };
        match req.get("type").and_then(|t| t.as_str()) {
            Some("prompt") => {
                // `text` muss ein nicht-leerer String sein — sonst klarer Fehler statt Leer-Prompt.
                let text = match req.get("text").and_then(|t| t.as_str()) {
                    Some(t) if !t.is_empty() => t,
                    _ => {
                        emit_rpc(&serde_json::json!({
                            "type": "error",
                            "message": "prompt: Feld 'text' (nicht-leerer String) fehlt"
                        }));
                        continue;
                    }
                };
                let on_event = |ev: AgentEvent| {
                    if let Some(v) = rpc_event(&ev) {
                        emit_rpc(&v);
                    }
                };
                // Frischer Token pro Prompt; Ctrl+C bricht den laufenden Prompt ab und beendet.
                let cancel = CancellationToken::new();
                let result = tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        cancel.cancel();
                        emit_rpc(&serde_json::json!({ "type": "error", "message": "aborted" }));
                        break;
                    }
                    r = agent.prompt(text, &on_event, cancel.clone()) => r,
                };
                match result {
                    Ok(()) => emit_rpc(&serde_json::json!({ "type": "done" })),
                    Err(e) => {
                        emit_rpc(&serde_json::json!({ "type": "error", "message": e.to_string() }))
                    }
                }
            }
            other => emit_rpc(&serde_json::json!({
                "type": "error",
                "message": format!("unbekannter request-typ: {}", other.unwrap_or("(fehlt)"))
            })),
        }
    }
    // Shutdown (EOF/Ctrl+C): Session abschließen (fsync), damit der Audit-Trail durabel ist.
    if let Err(e) = agent.finalize().await {
        emit_rpc(&serde_json::json!({ "type": "error", "message": format!("finalize: {e}") }));
    }
    Ok(())
}

fn emit_rpc(v: &serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

/// Mappt ein [`AgentEvent`] auf eine RPC-JSONL-Zeile (oder `None`, wenn nicht relevant).
fn rpc_event(ev: &AgentEvent) -> Option<serde_json::Value> {
    use serde_json::json;
    match ev {
        AgentEvent::TextDelta(t) => Some(json!({ "type": "text", "text": t })),
        AgentEvent::ThinkingDelta(t) => Some(json!({ "type": "thinking", "text": t })),
        AgentEvent::ToolStart { id, name } => {
            Some(json!({ "type": "tool_start", "id": id, "name": name }))
        }
        AgentEvent::ToolEnd { id, is_error } => {
            Some(json!({ "type": "tool_end", "id": id, "is_error": is_error }))
        }
        AgentEvent::Error(m) => Some(json!({ "type": "error", "message": m })),
        AgentEvent::TurnStart | AgentEvent::TurnEnd | AgentEvent::Done => None,
    }
}

/// Schreibt einen `aborted`-Audit-Eintrag in den Store, macht ihn durabel (fsync) und liefert den
/// Abbruch-Fehler zurück. So existiert die Session-Datei auch dann, wenn der Start vor dem ersten
/// Provider-Call scheitert (z. B. fehlender API-Key) — lückenloser Audit-Trail. Schreibfehler
/// werden bewusst geschluckt, damit der eigentliche Abbruchgrund (`msg`) nicht verdeckt wird.
async fn abort_with_audit(
    store: &mut dyn sepp_session::SessionStore,
    msg: &str,
    detail: serde_json::Value,
) -> anyhow::Error {
    let _ = store.append(sepp_session::EntryPayload::Custom {
        kind: "aborted".into(),
        data: detail,
    });
    let _ = store.flush().await;
    anyhow::anyhow!("{msg}")
}

/// Wählt das Session-Backend (JSONL-Default oder SQLite via `--sqlite`).
fn build_store(
    sqlite: bool,
    has_prompt: bool,
    rpc: bool,
    select: &SessionSelect,
) -> anyhow::Result<Box<dyn sepp_session::SessionStore>> {
    if sqlite {
        #[cfg(feature = "sqlite")]
        {
            if !has_prompt && !rpc {
                anyhow::bail!("--sqlite ist nur mit -p/--rpc nutzbar (die TUI nutzt JSONL)");
            }
            return session::sqlite_store(select);
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = (has_prompt, rpc);
            anyhow::bail!(
                "--sqlite: Binary ohne Feature 'sqlite' gebaut (cargo build -p sepp-cli --features sqlite)"
            );
        }
    }
    session::open_store(select)
}

/// Aussagekräftiges Modell-Label für die Anzeige: bevorzugt `display_name`, fällt aber auf die `id`
/// zurück, wenn das Modell der generische Custom-Platzhalter `(custom)` ist (so erscheint die
/// konkrete Modell-ID statt `(custom)` bei lokalen bzw. eigenen Modellen).
pub(crate) fn model_label(model: &Model) -> &str {
    if model.display_name == "(custom)" {
        &model.id
    } else {
        &model.display_name
    }
}

/// Fallback-`Model` für unregistrierte IDs — provider-bewusst (Kontextfenster, Provider-Tag).
/// Auch vom TUI-`/model`-Befehl genutzt; das Custom-Modell erbt dort den Session-Provider.
pub(crate) fn custom_model(id: String, provider: &str) -> Model {
    // Konservatives Kontextfenster je Provider (steuert die Auto-Compaction-Schwelle):
    // Anthropic 200k, OpenAI/lokal 128k (typisch) — lieber früher komprimieren als überlaufen.
    let context_window = if provider == "anthropic" {
        200_000
    } else {
        128_000
    };
    Model {
        id,
        provider: provider.to_string(),
        display_name: "(custom)".into(),
        context_window,
        max_output_tokens: 8192,
        supports_reasoning: true,
        supports_images: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_event_maps_relevant_events() {
        let v = rpc_event(&AgentEvent::TextDelta("hi".into())).unwrap();
        assert_eq!(v["type"], "text");
        assert_eq!(v["text"], "hi");

        let v = rpc_event(&AgentEvent::ToolStart {
            id: "t1".into(),
            name: "bash".into(),
        })
        .unwrap();
        assert_eq!(v["type"], "tool_start");
        assert_eq!(v["name"], "bash");

        let v = rpc_event(&AgentEvent::ToolEnd {
            id: "t1".into(),
            is_error: true,
        })
        .unwrap();
        assert_eq!(v["type"], "tool_end");
        assert_eq!(v["is_error"], true);

        // Lifecycle-Events erzeugen keine RPC-Zeile.
        assert!(rpc_event(&AgentEvent::TurnStart).is_none());
        assert!(rpc_event(&AgentEvent::Done).is_none());
    }

    #[test]
    fn openai_local_precheck_decides_early_aborts() {
        // openai: ohne base UND ohne Key → früher, klarer Abbruch statt rohem 401.
        assert_eq!(
            openai_local_precheck("openai", None, None),
            Some("missing_api_key")
        );
        // openai: Key ODER base_url reichen (lokale/kompatible Endpunkte bleiben key-optional).
        assert_eq!(openai_local_precheck("openai", None, Some("sk-x")), None);
        assert_eq!(
            openai_local_precheck("openai", Some("http://x/v1"), None),
            None
        );
        // local MEINT einen lokalen Endpunkt: ohne base_url wäre der from_env-Fallback auf
        // api.openai.com ein stiller Cloud-Egress — auch mit gesetztem Key abbrechen.
        assert_eq!(
            openai_local_precheck("local", None, Some("sk-x")),
            Some("missing_base_url")
        );
        assert_eq!(
            openai_local_precheck("local", Some("http://localhost:11434/v1"), None),
            None
        );
        // Andere Provider haben eigene Checks — hier kein Urteil.
        assert_eq!(openai_local_precheck("anthropic", None, None), None);
        assert_eq!(openai_local_precheck("mlx", None, None), None);
        assert_eq!(openai_local_precheck("zai", None, None), None);
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_init_only_as_first_arg() {
        // Ohne Flag: projektlokal.
        assert!(matches!(
            parse(&args(&["init"])).unwrap(),
            Cmd::Init {
                scope: session::InitScope::Project
            }
        ));
        // Nicht erstes Token → bleibt Prompt, nicht Subcommand.
        let cmd = parse(&args(&["-p", "init"])).unwrap();
        assert!(matches!(cmd, Cmd::Run(RunOpts { prompt: Some(p), .. }) if p == "init"));
    }

    #[test]
    fn parse_init_scope_flags() {
        use session::InitScope;
        assert!(matches!(
            parse(&args(&["init", "--global"])).unwrap(),
            Cmd::Init {
                scope: InitScope::Global
            }
        ));
        assert!(matches!(
            parse(&args(&["init", "-g"])).unwrap(),
            Cmd::Init {
                scope: InitScope::Global
            }
        ));
        assert!(matches!(
            parse(&args(&["init", "--system"])).unwrap(),
            Cmd::Init {
                scope: InitScope::System
            }
        ));
        // Explizite Default-Aliase.
        for flag in ["--here", "--local"] {
            assert!(matches!(
                parse(&args(&["init", flag])).unwrap(),
                Cmd::Init {
                    scope: InitScope::Project
                }
            ));
        }
        assert!(parse(&args(&["init", "--bogus"])).is_err());
    }

    #[test]
    fn parse_uninstall_flags() {
        assert!(matches!(
            parse(&args(&["uninstall"])).unwrap(),
            Cmd::Uninstall { purge: false }
        ));
        assert!(matches!(
            parse(&args(&["uninstall", "--purge"])).unwrap(),
            Cmd::Uninstall { purge: true }
        ));
        assert!(parse(&args(&["uninstall", "--bogus"])).is_err());
    }

    #[test]
    fn purge_targets_projects_then_both_global_roots() {
        // FHS-Fall: getrennte config/state-Roots, beide am Ende.
        let config = PathBuf::from("/etc/sepp");
        let state = PathBuf::from("/var/lib/sepp");
        let trusted = vec![PathBuf::from("/home/projA"), PathBuf::from("/srv/projB")];
        let cwd = PathBuf::from("/home/projA"); // bereits in trusted → kein Duplikat
        let t = purge_targets(&config, &state, &trusted, Some(&cwd));
        assert_eq!(
            t,
            vec![
                PathBuf::from("/home/projA/.sepp"),
                PathBuf::from("/srv/projB/.sepp"),
                PathBuf::from("/etc/sepp"),
                PathBuf::from("/var/lib/sepp"),
            ]
        );
    }

    #[test]
    fn purge_targets_dedups_single_root_default() {
        // `~/.sepp`-Default: config == state → globale Wurzel nur einmal; `init` aus dem Home
        // (<home>/.sepp == Wurzel) doppelt ebenfalls nicht.
        let root = PathBuf::from("/root/.sepp");
        let trusted = vec![PathBuf::from("/root")];
        let t = purge_targets(&root, &root, &trusted, None);
        assert_eq!(t, vec![PathBuf::from("/root/.sepp")]);
    }

    #[test]
    fn purge_targets_adds_untrusted_cwd() {
        let root = PathBuf::from("/root/.sepp");
        let t = purge_targets(&root, &root, &[], Some(Path::new("/tmp/here")));
        assert_eq!(
            t,
            vec![
                PathBuf::from("/tmp/here/.sepp"),
                PathBuf::from("/root/.sepp")
            ]
        );
    }

    #[test]
    fn init_config_is_idempotent_and_config_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".sepp");

        init_config_at(&root).unwrap();
        let settings = root.join("settings.toml");
        let first = std::fs::read_to_string(&settings).unwrap();
        for sub in ["skills", "prompts", "hooks", "plugins"] {
            assert!(root.join(sub).is_dir(), "{sub} sollte existieren");
        }
        // Config-only: KEIN sessions/ (zentral im state_root) und KEINE .gitignore mehr.
        assert!(
            !root.join("sessions").exists(),
            "sessions/ ist config-only nicht hier"
        );
        assert!(!root.join(".gitignore").exists(), "keine .gitignore mehr");

        // Zweiter Lauf: kein Fehler, settings.toml unverändert (Nutzerinhalt wird nie überschrieben).
        init_config_at(&root).unwrap();
        assert_eq!(first, std::fs::read_to_string(&settings).unwrap());
    }

    #[test]
    fn init_state_creates_sessions_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        init_state_at(&root, false).unwrap();
        assert!(
            root.join("sessions").is_dir(),
            "sessions/ sollte existieren"
        );
        // idempotent
        init_state_at(&root, false).unwrap();
    }

    #[tokio::test]
    async fn abort_with_audit_writes_durable_aborted_entry() {
        // Trait im Scope für `.entries()` auf dem konkreten Store. Der Abbruch-Pfad (z. B. fehlender
        // API-Key) muss eine durabel geschriebene `aborted`-Spur hinterlassen — ohne Subprozess/
        // Env-Gefummel, direkt auf dem Store getestet.
        use sepp_session::SessionStore;
        let dir = tempfile::tempdir().unwrap();
        let mut store = sepp_session::JsonlSessionStore::create(dir.path()).unwrap();
        let err = abort_with_audit(
            &mut store,
            "ANTHROPIC_API_KEY nicht gesetzt",
            serde_json::json!({ "reason": "missing_api_key", "provider": "anthropic" }),
        )
        .await;
        assert!(err.to_string().contains("ANTHROPIC_API_KEY"));

        // Datei reöffnen (Store lebt noch → prüft den fsync) und den Eintrag verifizieren.
        let infos = sepp_session::JsonlSessionStore::list(dir.path()).unwrap();
        let reopened = sepp_session::JsonlSessionStore::open(&infos[0].path).unwrap();
        let aborted = reopened.entries().iter().find_map(|e| match &e.payload {
            sepp_session::EntryPayload::Custom { kind, data } if kind == "aborted" => {
                Some(data.clone())
            }
            _ => None,
        });
        let data = aborted.expect("ein `aborted`-Eintrag sollte persistiert sein");
        assert_eq!(data["reason"], "missing_api_key");
        assert_eq!(data["provider"], "anthropic");
    }

    #[test]
    fn resolve_thinking_defaults_and_precedence() {
        // Provider-Default: z.ai an, sonst aus.
        assert_eq!(resolve_thinking(None, None, true), ThinkingLevel::Medium);
        assert_eq!(resolve_thinking(None, None, false), ThinkingLevel::Off);
        // Explizite Flags überall.
        assert_eq!(
            resolve_thinking(Some(true), None, false),
            ThinkingLevel::Medium
        );
        assert_eq!(
            resolve_thinking(Some(false), None, true),
            ThinkingLevel::Off
        );
        // Env greift, wenn kein Flag.
        assert_eq!(resolve_thinking(None, Some("0"), true), ThinkingLevel::Off);
        assert_eq!(
            resolve_thinking(None, Some("on"), false),
            ThinkingLevel::Medium
        );
        // Flag schlägt Env.
        assert_eq!(
            resolve_thinking(Some(false), Some("1"), true),
            ThinkingLevel::Off
        );
        // Unbekannter Env-Wert → ignoriert → Provider-Default.
        assert_eq!(
            resolve_thinking(None, Some("vielleicht"), true),
            ThinkingLevel::Medium
        );
    }

    #[test]
    fn parse_think_flags() {
        let on = parse(&args(&["--think", "-p", "x"])).unwrap();
        assert!(matches!(
            on,
            Cmd::Run(RunOpts {
                think: Some(true),
                ..
            })
        ));
        let off = parse(&args(&["--no-think", "-p", "x"])).unwrap();
        assert!(matches!(
            off,
            Cmd::Run(RunOpts {
                think: Some(false),
                ..
            })
        ));
        let hide = parse(&args(&["--hide-thinking", "-p", "x"])).unwrap();
        assert!(matches!(
            hide,
            Cmd::Run(RunOpts {
                hide_thinking: true,
                think: None,
                ..
            })
        ));
        // Default: kein Flag.
        let def = parse(&args(&["-p", "x"])).unwrap();
        assert!(matches!(
            def,
            Cmd::Run(RunOpts {
                think: None,
                hide_thinking: false,
                ..
            })
        ));
    }
}
