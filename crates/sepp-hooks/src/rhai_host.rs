//! Rhai-basierter [`HookHost`]: lädt `*.rhai` aus Verzeichnissen und ruft pro Hook-Punkt die
//! passende `on_*`-Funktion auf.
//!
//! **Ein Skript, das scheitert, sagt es.** Rhai meldet „Funktion nicht gefunden" für zwei sehr
//! verschiedene Dinge: Das Skript definiert den Handler nicht (dann ist Überspringen richtig),
//! oder der Handler existiert und ruft in seinem Rumpf etwas Falsches auf (`handled("x")` — die
//! Funktion nimmt kein Argument). Bis 0.5.2 galt beides als „kein Handler", und ein Tippfehler
//! schaltete den Hook stumm ab; das war die häufigste Ursache für „mein Hook tut nichts".
//! Unterschieden wird an der Nutzlast, siehe [`missing_handler`].
//!
//! Meldungen sammelt der Host in einem Puffer, den der Loop über
//! [`HookHost::drain_notices`] abholt und als Hinweis anzeigt — `tracing` allein erreicht
//! niemanden (in der TUI gibt es keinen Subscriber, im One-shot filtert der Default `warn`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rhai::{Dynamic, Engine, EvalAltResult, FuncArgs, Map, Scope, AST};

use sepp_core::{Result, SeppError};

use crate::{HookEvent, HookHost, HookOutcome};

/// Die Hook-Punkte mit ihrer Parameterzahl. Rhai sucht einen Handler nach Name **und** Arität:
/// `fn on_tool_call(name)` mit einem Parameter statt zweien ist zur Laufzeit von „gibt es nicht"
/// nicht zu unterscheiden. Deshalb wird beim Laden dagegen geprüft.
const HANDLERS: &[(&str, usize)] = &[
    ("on_before_agent_start", 1),
    ("on_input", 1),
    ("on_tool_call", 2),
    ("on_tool_result", 2),
    ("on_turn_end", 1),
];

/// Lädt und führt Rhai-Hook-Skripte aus.
pub struct RhaiHookHost {
    engine: Engine,
    scripts: Vec<(String, AST)>,
    /// Meldungen für den Loop. `Arc`, weil die `notify`/`log`-Closures der Engine hineinschreiben.
    notices: Arc<Mutex<Vec<String>>>,
    /// Schon gemeldete `(Skript, Handler)`-Paare. Ein Hook, der bei jedem der zwanzig Werkzeuge
    /// eines Batches scheitert, soll den Menschen einmal warnen, nicht zwanzigmal.
    reported: Mutex<HashSet<(String, String)>>,
}

impl RhaiHookHost {
    /// Host ohne Skripte (gegatete Engine).
    pub fn new() -> Self {
        let notices: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        RhaiHookHost {
            engine: build_engine(notices.clone()),
            scripts: Vec::new(),
            notices,
            reported: Mutex::new(HashSet::new()),
        }
    }

    /// Legt eine Meldung in den Puffer, den der Loop abholt.
    fn note(&self, msg: String) {
        if let Ok(mut v) = self.notices.lock() {
            v.push(msg);
        }
    }

    /// Wie [`Self::note`], aber höchstens einmal je `(Skript, Handler)`.
    fn note_once(&self, script: &str, handler: &str, msg: String) {
        let key = (script.to_string(), handler.to_string());
        let fresh = match self.reported.lock() {
            Ok(mut seen) => seen.insert(key),
            // Ein vergifteter Mutex darf keine Meldung verschlucken.
            Err(_) => true,
        };
        if fresh {
            self.note(msg);
        }
    }

    /// Kompiliert alle `*.rhai` aus den angegebenen Verzeichnissen (fehlende werden ignoriert).
    pub fn from_dirs(dirs: &[PathBuf]) -> Result<Self> {
        let mut host = RhaiHookHost::new();
        for dir in dirs {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut entries: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("rhai"))
                .collect();
            entries.sort();
            for path in entries {
                let src = std::fs::read_to_string(&path)?;
                let ast = host.engine.compile(&src).map_err(|e| {
                    SeppError::Config(format!("rhai compile {}: {e}", path.display()))
                })?;
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("hook")
                    .to_string();
                for msg in check_handlers(&name, &ast) {
                    host.note(msg);
                }
                host.scripts.push((name, ast));
            }
        }
        Ok(host)
    }

    pub fn is_empty(&self) -> bool {
        self.scripts.is_empty()
    }

    pub fn script_count(&self) -> usize {
        self.scripts.len()
    }

    /// Ruft eine Handler-Funktion auf.
    ///
    /// `Ok(None)`: Das Skript definiert den Handler nicht — der Normalfall, jedes Skript
    /// bedient nur die Punkte, die es braucht. `Err`: Der Handler existiert und ist gescheitert;
    /// die Meldung liegt dann bereits im Puffer, der Aufrufer entscheidet, ob er sie zusätzlich
    /// nach oben reicht.
    fn call(
        &self,
        script: &str,
        ast: &AST,
        name: &str,
        args: impl FuncArgs,
    ) -> Result<Option<Dynamic>> {
        let mut scope = Scope::new();
        match self.engine.call_fn::<Dynamic>(&mut scope, ast, name, args) {
            Ok(d) => Ok(Some(d)),
            Err(e) if missing_handler(&e, name) => Ok(None),
            Err(e) => {
                self.note_once(
                    script,
                    name,
                    format!("Hook {script}: {name} ist gescheitert — {e}"),
                );
                Err(SeppError::Config(format!("rhai {script}/{name}: {e}")))
            }
        }
    }

    /// Ruft einen Handler und **verschluckt** einen Skriptfehler, weil er schon gemeldet wurde.
    ///
    /// Für die Hook-Punkte, an denen ein kaputtes Skript die Arbeit nicht aufhalten soll. Die
    /// Ausnahme ist `ToolCall`: Dort hätte der Hook `block` sagen können, ihn zu überspringen
    /// *und* das Werkzeug auszuführen würde eine beabsichtigte Blockade in eine Ausführung
    /// verwandeln.
    fn call_lenient(
        &self,
        script: &str,
        ast: &AST,
        name: &str,
        args: impl FuncArgs,
    ) -> Option<Dynamic> {
        self.call(script, ast, name, args).ok().flatten()
    }
}

/// Ist das der Fehler „dieses Skript definiert den Handler nicht"?
///
/// Rhai benutzt `ErrorFunctionNotFound` für zwei Fälle und unterscheidet sie in **beiden**
/// Feldern der Nutzlast:
///
/// * Handler fehlt — der blanke Name und [`rhai::Position::NONE`].
/// * Aufruf im Rumpf gescheitert — eine *Signatur* mit Klammern und Argumenttypen
///   (`handled (&str | ImmutableString | String)`) und die echte Quellposition.
///
/// Beides zu prüfen ist Absicht: Jedes Signal für sich wäre eine Vermutung, zusammen sind sie
/// eindeutig. Der Test `rhai_marks_the_two_not_found_cases_differently` nagelt das Format fest —
/// es gehört einer fremden Crate, und ein stiller Wechsel dort würde den Hook wieder verstummen
/// lassen.
fn missing_handler(e: &EvalAltResult, handler: &str) -> bool {
    match e {
        EvalAltResult::ErrorFunctionNotFound(sig, pos) => sig == handler && pos.is_none(),
        _ => false,
    }
}

/// Prüft beim Laden, ob die Handler eines Skripts überhaupt gerufen werden können.
///
/// Zwei Fälle, die zur Laufzeit von „Handler nicht definiert" nicht zu unterscheiden wären und
/// den Hook deshalb stumm abschalten würden: die falsche Parameterzahl und ein vertippter
/// Handler-Name. Beides kostet hier eine Schleife und erspart eine lange Fehlersuche.
fn check_handlers(script: &str, ast: &AST) -> Vec<String> {
    let mut out = Vec::new();
    for f in ast.iter_functions() {
        match HANDLERS.iter().find(|(n, _)| *n == f.name) {
            Some((n, want)) if f.params.len() != *want => out.push(format!(
                "Hook {script}: {n} hat {} Parameter, erwartet sind {want} — Rhai findet den \
                 Handler so nicht und überspringt ihn",
                f.params.len()
            )),
            Some(_) => {}
            None if f.name.starts_with("on_") => out.push(format!(
                "Hook {script}: {} ist kein Hook-Punkt und wird nie gerufen (bekannt sind {})",
                f.name,
                HANDLERS
                    .iter()
                    .map(|(n, _)| *n)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            None => {}
        }
    }
    out
}

impl Default for RhaiHookHost {
    fn default() -> Self {
        Self::new()
    }
}

impl HookHost for RhaiHookHost {
    fn drain_notices(&self) -> Vec<String> {
        match self.notices.lock() {
            Ok(mut v) => std::mem::take(&mut *v),
            Err(_) => Vec::new(),
        }
    }

    fn dispatch(&self, event: HookEvent<'_>) -> Result<HookOutcome> {
        match event {
            HookEvent::BeforeAgentStart { system_prompt } => {
                let mut cur = system_prompt.clone();
                for (script, ast) in &self.scripts {
                    if let Some(ret) =
                        self.call_lenient(script, ast, "on_before_agent_start", (cur.clone(),))
                    {
                        if let Some(s) = dynamic_string(&ret) {
                            cur = s;
                        }
                    }
                }
                *system_prompt = cur;
                Ok(HookOutcome::Continue)
            }
            HookEvent::Input { text } => {
                let mut cur = text.clone();
                for (script, ast) in &self.scripts {
                    if let Some(ret) = self.call_lenient(script, ast, "on_input", (cur.clone(),)) {
                        match as_outcome(&ret) {
                            Some(HookOutcome::Handled) => {
                                *text = cur;
                                return Ok(HookOutcome::Handled);
                            }
                            Some(_) => {}
                            None => {
                                if let Some(s) = dynamic_string(&ret) {
                                    cur = s;
                                }
                            }
                        }
                    }
                }
                *text = cur;
                Ok(HookOutcome::Continue)
            }
            HookEvent::ToolCall { name, input } => {
                let dyn_input = rhai::serde::to_dynamic(&*input)
                    .map_err(|e| SeppError::Config(format!("rhai to_dynamic(input): {e}")))?;
                for (script, ast) in &self.scripts {
                    // Hier **nicht** nachsichtig: Ein gescheiterter Hook hätte `block` sagen
                    // können. Der Fehler wird zum Ergebnis des Werkzeugs, der Aufruf läuft nicht.
                    if let Some(ret) = self.call(
                        script,
                        ast,
                        "on_tool_call",
                        (name.to_string(), dyn_input.clone()),
                    )? {
                        match as_outcome(&ret) {
                            Some(HookOutcome::Continue) | None => {}
                            Some(other) => return Ok(other),
                        }
                    }
                }
                Ok(HookOutcome::Continue)
            }
            HookEvent::ToolResult { name, result } => {
                // Beobachtend (Transform via Hooks ist Phase 4+).
                let dyn_res = rhai::serde::to_dynamic(&*result).unwrap_or(Dynamic::UNIT);
                for (script, ast) in &self.scripts {
                    self.call_lenient(
                        script,
                        ast,
                        "on_tool_result",
                        (name.to_string(), dyn_res.clone()),
                    );
                }
                Ok(HookOutcome::Continue)
            }
            HookEvent::TurnEnd { message } => {
                let dyn_msg = rhai::serde::to_dynamic(message).unwrap_or(Dynamic::UNIT);
                for (script, ast) in &self.scripts {
                    self.call_lenient(script, ast, "on_turn_end", (dyn_msg.clone(),));
                }
                Ok(HookOutcome::Continue)
            }
        }
    }
}

fn build_engine(notices: Arc<Mutex<Vec<String>>>) -> Engine {
    let mut engine = Engine::new();

    engine.register_fn("block", |reason: rhai::ImmutableString| -> Map {
        let mut m = Map::new();
        m.insert("__hook".into(), Dynamic::from("block"));
        m.insert("reason".into(), Dynamic::from(reason));
        m
    });
    engine.register_fn("continue_", || -> Map {
        let mut m = Map::new();
        m.insert("__hook".into(), Dynamic::from("continue"));
        m
    });
    engine.register_fn("handled", || -> Map {
        let mut m = Map::new();
        m.insert("__hook".into(), Dynamic::from("handled"));
        m
    });
    // Gegatete Host-API — KEIN fs/net. `notify` ist der Kanal des Hook-Autors zum Menschen und
    // geht deshalb in den Meldungspuffer, den der Loop abholt; `tracing` bleibt zusätzlich für
    // `RUST_LOG`. Über tracing allein kam die Nachricht nirgends an (TUI ohne Subscriber,
    // One-shot filtert `warn`). `log` bleibt reines Diagnosewerkzeug und damit still.
    let sink = notices.clone();
    engine.register_fn("notify", move |msg: rhai::ImmutableString| {
        tracing::info!(target: "hook", "{msg}");
        if let Ok(mut v) = sink.lock() {
            v.push(format!("Hook: {msg}"));
        }
    });
    engine.register_fn("log", |msg: rhai::ImmutableString| {
        tracing::debug!(target: "hook", "{msg}");
    });
    // Rhais eingebaute `print`/`debug` würden auf stdout schreiben — stdout ist der Datenkanal
    // (One-shot, RPC). Deshalb nach tracing umleiten (im TUI ohne Subscriber = No-op).
    engine.on_print(|s| tracing::info!(target: "hook", "{s}"));
    engine.on_debug(|s, src, pos| {
        tracing::debug!(target: "hook", "{}@{pos}: {s}", src.unwrap_or("?"));
    });

    // Ressourcen begrenzen (Engine-Sandbox).
    engine.set_max_operations(500_000);
    engine.set_max_call_levels(32);
    engine.set_max_string_size(200_000);
    engine.set_max_array_size(10_000);
    engine
}

fn dynamic_string(d: &Dynamic) -> Option<String> {
    d.clone().into_string().ok()
}

fn as_outcome(d: &Dynamic) -> Option<HookOutcome> {
    let map = d.clone().try_cast::<Map>()?;
    let tag = map.get("__hook")?.clone().into_string().ok()?;
    match tag.as_str() {
        "block" => Some(HookOutcome::Block {
            reason: map
                .get("reason")
                .and_then(|v| v.clone().into_string().ok())
                .unwrap_or_else(|| "blockiert".into()),
        }),
        "handled" => Some(HookOutcome::Handled),
        "continue" => Some(HookOutcome::Continue),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn host_with(script: &str) -> RhaiHookHost {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("h.rhai"), script).unwrap();
        // tempdir lebt nur in diesem Scope; from_dirs liest sofort ein → ok.
        RhaiHookHost::from_dirs(&[tmp.path().to_path_buf()]).unwrap()
    }

    #[test]
    fn tool_call_hook_blocks_rm_rf() {
        let host = host_with(
            r#"
            fn on_tool_call(name, input) {
                if name == "bash" && input.command.contains("rm -rf") {
                    return block("rm -rf ist blockiert");
                }
                continue_()
            }
            "#,
        );
        let mut input = json!({ "command": "rm -rf /" });
        let outcome = host
            .dispatch(HookEvent::ToolCall {
                name: "bash",
                input: &mut input,
            })
            .unwrap();
        assert!(matches!(outcome, HookOutcome::Block { reason } if reason.contains("rm -rf")));

        // Harmloses Kommando läuft durch.
        let mut ok = json!({ "command": "ls" });
        let outcome = host
            .dispatch(HookEvent::ToolCall {
                name: "bash",
                input: &mut ok,
            })
            .unwrap();
        assert!(matches!(outcome, HookOutcome::Continue));
    }

    #[test]
    fn input_hook_transforms_text() {
        let host = host_with(
            r#"
            fn on_input(text) {
                text + " (geprüft)"
            }
            "#,
        );
        let mut text = String::from("baue das feature");
        let outcome = host.dispatch(HookEvent::Input { text: &mut text }).unwrap();
        assert!(matches!(outcome, HookOutcome::Continue));
        assert_eq!(text, "baue das feature (geprüft)");
    }

    #[test]
    fn print_and_debug_in_hook_do_not_fail() {
        // `print`/`debug` gehen nach tracing statt stdout — der Hook läuft normal durch.
        let host = host_with(
            r#"
            fn on_input(text) {
                print("hallo");
                debug("x");
                text
            }
            "#,
        );
        let mut text = String::from("a");
        let outcome = host.dispatch(HookEvent::Input { text: &mut text }).unwrap();
        assert!(matches!(outcome, HookOutcome::Continue));
        assert_eq!(text, "a");
    }

    #[test]
    fn missing_handlers_are_noop() {
        let host = host_with("fn unrelated() { 1 }");
        let mut input = json!({ "command": "rm -rf /" });
        let outcome = host
            .dispatch(HookEvent::ToolCall {
                name: "bash",
                input: &mut input,
            })
            .unwrap();
        assert!(matches!(outcome, HookOutcome::Continue));
        assert!(
            host.drain_notices().is_empty(),
            "ein Skript ohne den Handler ist der Normalfall und keine Meldung wert"
        );
    }

    /// Das Signal, auf dem der ganze Fix steht: Rhai unterscheidet „Handler fehlt" von
    /// „Aufruf im Rumpf gescheitert" — im Namen **und** in der Position. Beides gehört einer
    /// fremden Crate; änderte es sich still, würden Hooks wieder stumm verstummen.
    #[test]
    fn rhai_marks_the_two_not_found_cases_differently() {
        let host = host_with(r#"fn on_input(text) { handled("x") }"#);
        let (_, ast) = &host.scripts[0];
        let mut scope = Scope::new();

        // (a) Der Handler fehlt: blanker Name, keine Position.
        let e = host
            .engine
            .call_fn::<Dynamic>(&mut scope, ast, "on_turn_end", ("x".to_string(),))
            .expect_err("on_turn_end ist nicht definiert");
        match &*e {
            EvalAltResult::ErrorFunctionNotFound(sig, pos) => {
                assert_eq!(sig, "on_turn_end", "blanker Name erwartet");
                assert!(pos.is_none(), "keine Quellposition erwartet, war {pos:?}");
            }
            other => panic!("unerwartete Variante: {other:?}"),
        }
        assert!(missing_handler(&e, "on_turn_end"));

        // (b) Der Handler ist da, aber `handled` nimmt kein Argument: Signatur mit Klammern
        // und echte Position.
        let mut scope = Scope::new();
        let e = host
            .engine
            .call_fn::<Dynamic>(&mut scope, ast, "on_input", ("x".to_string(),))
            .expect_err("handled(\"x\") gibt es nicht");
        match &*e {
            EvalAltResult::ErrorFunctionNotFound(sig, pos) => {
                assert!(
                    sig.starts_with("handled ("),
                    "Signatur erwartet, war {sig:?}"
                );
                assert!(!pos.is_none(), "Quellposition erwartet");
            }
            other => panic!("unerwartete Variante: {other:?}"),
        }
        assert!(
            !missing_handler(&e, "on_input"),
            "ein Tippfehler im Rumpf ist kein fehlender Handler"
        );
    }

    #[test]
    fn a_typo_inside_the_handler_is_reported_with_the_script_name() {
        let host = host_with(r#"fn on_input(text) { handled("erledigt") }"#);
        let mut text = String::from("hallo");

        // Der Loop läuft weiter: Die Eingabe bleibt, wie sie war.
        let outcome = host
            .dispatch(HookEvent::Input { text: &mut text })
            .expect("ein kaputter Hook hält die Arbeit nicht auf");
        assert!(matches!(outcome, HookOutcome::Continue));
        assert_eq!(text, "hallo");

        let notes = host.drain_notices();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("h.rhai"),
            "Skriptname fehlt: {}",
            notes[0]
        );
        assert!(notes[0].contains("on_input"), "Handler fehlt: {}", notes[0]);
        assert!(notes[0].contains("handled"), "Ursache fehlt: {}", notes[0]);
        assert!(
            host.drain_notices().is_empty(),
            "abgeholte Meldungen sind weg"
        );
    }

    #[test]
    fn the_same_broken_handler_is_reported_once_per_session() {
        let host = host_with(r#"fn on_tool_result(name, result) { notify() }"#);
        let mut tr = sepp_core::ToolResult::text("ok");
        for _ in 0..5 {
            let _ = host.dispatch(HookEvent::ToolResult {
                name: "bash",
                result: &mut tr,
            });
        }
        assert_eq!(
            host.drain_notices().len(),
            1,
            "ein Hook, der bei jedem Werkzeug scheitert, warnt einmal — nicht bei jedem Aufruf"
        );
    }

    #[test]
    fn a_failing_tool_call_hook_stops_the_tool() {
        // Bewusste Ausnahme: Hier hätte der Hook `block` sagen können. Ihn zu überspringen und
        // das Werkzeug trotzdem auszuführen würde eine beabsichtigte Blockade zur Ausführung.
        let host = host_with(r#"fn on_tool_call(name, input) { block() }"#);
        let mut input = json!({ "command": "ls" });
        let err = host
            .dispatch(HookEvent::ToolCall {
                name: "bash",
                input: &mut input,
            })
            .expect_err("der Aufruf darf nicht durchgehen");
        assert!(err.to_string().contains("h.rhai"), "{err}");
        assert_eq!(host.drain_notices().len(), 1);
    }

    #[test]
    fn wrong_arity_and_misspelled_handlers_are_caught_while_loading() {
        // Rhai sucht nach Name UND Parameterzahl — zur Laufzeit wäre beides von „Handler nicht
        // definiert" nicht zu unterscheiden.
        let host = host_with("fn on_tool_call(name) { 1 }\nfn on_inpt(text) { text }\n");
        let notes = host.drain_notices();
        assert_eq!(notes.len(), 2, "{notes:?}");
        let all = notes.join("\n");
        assert!(all.contains("on_tool_call"), "{all}");
        assert!(all.contains("1 Parameter"), "{all}");
        assert!(all.contains("on_inpt"), "{all}");
        assert!(all.contains("kein Hook-Punkt"), "{all}");
    }

    #[test]
    fn a_correct_script_loads_without_a_word() {
        let host = host_with(
            "fn on_input(text) { text }\nfn on_tool_call(name, input) { continue_() }\nfn hilf() { 1 }\n",
        );
        assert!(host.drain_notices().is_empty());
        assert_eq!(host.script_count(), 1);
    }

    #[test]
    fn notify_reaches_the_loop_not_just_the_log() {
        let host = host_with(r#"fn on_input(text) { notify("Regel greift"); text }"#);
        let mut text = String::from("x");
        host.dispatch(HookEvent::Input { text: &mut text }).unwrap();
        let notes = host.drain_notices();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("Regel greift"), "{}", notes[0]);
    }
}
