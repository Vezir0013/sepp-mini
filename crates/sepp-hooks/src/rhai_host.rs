//! Rhai-basierter [`HookHost`]: lädt `*.rhai` aus Verzeichnissen und ruft pro Hook-Punkt die
//! passende `on_*`-Funktion auf.

use std::path::PathBuf;

use rhai::{Dynamic, Engine, EvalAltResult, FuncArgs, Map, Scope, AST};

use sepp_core::{Result, SeppError};

use crate::{HookEvent, HookHost, HookOutcome};

/// Lädt und führt Rhai-Hook-Skripte aus.
pub struct RhaiHookHost {
    engine: Engine,
    scripts: Vec<(String, AST)>,
}

impl RhaiHookHost {
    /// Host ohne Skripte (gegatete Engine).
    pub fn new() -> Self {
        RhaiHookHost {
            engine: build_engine(),
            scripts: Vec::new(),
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

    /// Ruft eine Handler-Funktion auf; `Ok(None)`, wenn das Skript sie nicht definiert.
    fn call(&self, ast: &AST, name: &str, args: impl FuncArgs) -> Result<Option<Dynamic>> {
        let mut scope = Scope::new();
        match self.engine.call_fn::<Dynamic>(&mut scope, ast, name, args) {
            Ok(d) => Ok(Some(d)),
            Err(e) if matches!(*e, EvalAltResult::ErrorFunctionNotFound(..)) => Ok(None),
            Err(e) => Err(SeppError::Config(format!("rhai {name}: {e}"))),
        }
    }
}

impl Default for RhaiHookHost {
    fn default() -> Self {
        Self::new()
    }
}

impl HookHost for RhaiHookHost {
    fn dispatch(&self, event: HookEvent<'_>) -> Result<HookOutcome> {
        match event {
            HookEvent::BeforeAgentStart { system_prompt } => {
                let mut cur = system_prompt.clone();
                for (_, ast) in &self.scripts {
                    if let Some(ret) = self.call(ast, "on_before_agent_start", (cur.clone(),))? {
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
                for (_, ast) in &self.scripts {
                    if let Some(ret) = self.call(ast, "on_input", (cur.clone(),))? {
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
                for (_, ast) in &self.scripts {
                    if let Some(ret) =
                        self.call(ast, "on_tool_call", (name.to_string(), dyn_input.clone()))?
                    {
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
                for (_, ast) in &self.scripts {
                    self.call(ast, "on_tool_result", (name.to_string(), dyn_res.clone()))?;
                }
                Ok(HookOutcome::Continue)
            }
            HookEvent::TurnEnd { message } => {
                let dyn_msg = rhai::serde::to_dynamic(message).unwrap_or(Dynamic::UNIT);
                for (_, ast) in &self.scripts {
                    self.call(ast, "on_turn_end", (dyn_msg.clone(),))?;
                }
                Ok(HookOutcome::Continue)
            }
        }
    }
}

fn build_engine() -> Engine {
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
    // Gegatete Host-API — KEIN fs/net. notify/log laufen über tracing (im TUI ohne
    // Subscriber = No-op, also kein stderr-Müll; im One-shot nach stderr).
    engine.register_fn("notify", |msg: rhai::ImmutableString| {
        tracing::info!(target: "hook", "{msg}");
    });
    engine.register_fn("log", |msg: rhai::ImmutableString| {
        tracing::debug!(target: "hook", "{msg}");
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
    }
}
