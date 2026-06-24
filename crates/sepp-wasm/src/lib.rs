//! `sepp-wasm` — Tier-2-Plugin-Host: lädt WASM-Plugins als Tools, **capability-gated**.
//!
//! Sicherheit: WASM ist von Natur aus capability-secure — ein Modul kann nur, was der Host als
//! Funktion bereitstellt. Host-Funktionen werden **nur registriert, wenn die Policy sie erlaubt**
//! (`docs/06-security-model.md`). Ein Plugin ohne `Net`-Capability importiert `host_http`, das
//! dann nicht existiert → Instanziierung schlägt fehl → es kann nachweislich nicht ins Netz.
//!
//! Plugin-ABI (Exports): `sepp_alloc(i32)->i32`, `sepp_spec()->i64`, `sepp_call(i32,i32)->i64`.
//! Der Rückgabewert `i64` packt `(ptr<<32 | len)`. `sepp_spec` liefert ToolSpec-JSON, `sepp_call`
//! erhält die Argument-JSON und liefert ToolResult-JSON (beides im linearen Speicher).
//! Gegatete Host-Importe (`env`-Modul): `host_log(i32,i32)` (immer), `host_fs_read`/`host_http`
//! (nur bei passender Capability).

use std::path::Path;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use wasmi::{Caller, Engine, Extern, Linker, Memory, Module, Store};

use sepp_core::{Result, SeppError, ToolResult, ToolSpec};
use sepp_policy::{Capability, Manifest, Policy};
use sepp_tools::Tool;

/// Pro-Instanz-Zustand (für Host-Funktionen).
#[derive(Default)]
struct HostState {
    logs: Vec<String>,
}

/// Obergrenze für Plugin-Rückgaben (ToolSpec-/ToolResult-JSON), damit ein bösartiges Plugin
/// den Host nicht durch eine riesige `len` zu einer GB-Allokation zwingt.
const MAX_PLUGIN_BYTES: u32 = 16 * 1024 * 1024;

fn unpack(v: i64) -> (u32, u32) {
    (((v >> 32) & 0xffff_ffff) as u32, (v & 0xffff_ffff) as u32)
}

fn read_mem(mem: &Memory, store: &Store<HostState>, ptr: u32, len: u32) -> Result<Vec<u8>> {
    if len > MAX_PLUGIN_BYTES {
        return Err(SeppError::Tool(format!(
            "wasm: Rückgabe zu groß ({len} > {MAX_PLUGIN_BYTES} Bytes)"
        )));
    }
    let (a, b) = (ptr as usize, ptr as usize + len as usize);
    mem.data(store)
        .get(a..b)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| SeppError::Tool("wasm: ungültiger Speicherbereich".into()))
}

fn build_linker(engine: &Engine, policy: &Policy) -> Result<Linker<HostState>> {
    let mut linker = Linker::<HostState>::new(engine);

    // host_log: immer verfügbar (gegatete Host-API, kein fs/net).
    linker
        .func_wrap(
            "env",
            "host_log",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| {
                if let Some(Extern::Memory(mem)) = caller.get_export("memory") {
                    let (a, b) = (ptr as usize, ptr as usize + len as usize);
                    let msg = mem
                        .data(&caller)
                        .get(a..b)
                        .map(|s| String::from_utf8_lossy(s).into_owned());
                    if let Some(msg) = msg {
                        tracing::info!(target: "wasm", "{msg}");
                        caller.data_mut().logs.push(msg);
                    }
                }
            },
        )
        .map_err(|e| SeppError::Tool(format!("wasm linker host_log: {e}")))?;

    // host_fs_read: nur mit FsRead-Capability (Stub — echte Implementierung folgt).
    if policy
        .granted
        .iter()
        .any(|c| matches!(c, Capability::FsRead { .. }))
    {
        linker
            .func_wrap(
                "env",
                "host_fs_read",
                |_c: Caller<'_, HostState>, _p: i32, _l: i32| -> i64 { 0 },
            )
            .map_err(|e| SeppError::Tool(format!("wasm linker host_fs_read: {e}")))?;
    }
    // host_http: nur mit Net-Capability (Stub) — DAS ist das Capability-Gate.
    if policy
        .granted
        .iter()
        .any(|c| matches!(c, Capability::Net { .. }))
    {
        linker
            .func_wrap(
                "env",
                "host_http",
                |_c: Caller<'_, HostState>, _p: i32, _l: i32| -> i64 { 0 },
            )
            .map_err(|e| SeppError::Tool(format!("wasm linker host_http: {e}")))?;
    }

    Ok(linker)
}

/// Der WASM-Host (hält die `wasmi`-Engine).
#[derive(Default)]
pub struct WasmHost {
    engine: Engine,
}

impl WasmHost {
    pub fn new() -> Self {
        WasmHost {
            engine: Engine::default(),
        }
    }

    /// Lädt ein Plugin aus WASM-Bytes mit der gegebenen Policy. Instanziiert einmal, um die
    /// `ToolSpec` zu holen (scheitert, wenn Capability-gegatete Importe fehlen → Gate).
    pub fn load(&self, wasm: &[u8], policy: Policy) -> Result<WasmPlugin> {
        let module = Module::new(&self.engine, wasm)
            .map_err(|e| SeppError::Tool(format!("wasm compile: {e}")))?;

        let mut store = Store::new(&self.engine, HostState::default());
        let linker = build_linker(&self.engine, &policy)?;
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|e| SeppError::Tool(format!("wasm instantiate: {e}")))?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| SeppError::Tool("wasm: kein 'memory'-Export".into()))?;
        let spec_fn = instance
            .get_typed_func::<(), i64>(&store, "sepp_spec")
            .map_err(|e| SeppError::Tool(format!("wasm: sepp_spec fehlt: {e}")))?;
        let packed = spec_fn
            .call(&mut store, ())
            .map_err(|e| SeppError::Tool(format!("wasm sepp_spec: {e}")))?;
        let (ptr, len) = unpack(packed);
        let bytes = read_mem(&memory, &store, ptr, len)?;
        let spec: ToolSpec = serde_json::from_slice(&bytes)
            .map_err(|e| SeppError::Tool(format!("wasm spec-json: {e}")))?;

        Ok(WasmPlugin {
            engine: self.engine.clone(),
            module,
            policy,
            spec,
        })
    }

    /// Lädt ein Plugin aus einer Datei; Capabilities aus dem (optionalen) Manifest.
    pub fn load_file(&self, wasm_path: &Path, manifest_path: Option<&Path>) -> Result<WasmPlugin> {
        let wasm = std::fs::read(wasm_path)
            .map_err(|e| SeppError::Tool(format!("wasm read {}: {e}", wasm_path.display())))?;
        let policy = match manifest_path {
            Some(p) => Manifest::from_file(p)?.policy(),
            None => Policy::default(),
        };
        self.load(&wasm, policy)
    }

    /// Findet `*.wasm` in `dir` (Manifest: `<stem>.toml` oder `manifest.toml` daneben) und lädt
    /// sie. Fehlerhafte Plugins werden übersprungen (geloggt).
    pub fn discover(&self, dir: &Path) -> Vec<WasmPlugin> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) != Some("wasm") {
                continue;
            }
            let stem_manifest = path.with_extension("toml");
            let dir_manifest = path.with_file_name("manifest.toml");
            let manifest = if stem_manifest.exists() {
                Some(stem_manifest)
            } else if dir_manifest.exists() {
                Some(dir_manifest)
            } else {
                None
            };
            match self.load_file(&path, manifest.as_deref()) {
                Ok(p) => out.push(p),
                Err(e) => tracing::warn!("wasm-plugin {} übersprungen: {e}", path.display()),
            }
        }
        out
    }
}

/// Ein geladenes WASM-Plugin, exponiert als [`Tool`].
pub struct WasmPlugin {
    engine: Engine,
    module: Module,
    policy: Policy,
    spec: ToolSpec,
}

impl WasmPlugin {
    /// Überschreibt den exponierten Tool-Namen (für Kollisions-Präfixe im gemeinsamen Toolset).
    pub fn rename(&mut self, name: String) {
        self.spec.label = name.clone();
        self.spec.name = name;
    }

    /// Synchroner Plugin-Lauf. Assoziierte Funktion (kein `&self`), damit `execute` sie per
    /// `spawn_blocking` in den Blocking-Pool auslagern kann (der Reactor bleibt frei).
    fn run(engine: &Engine, module: &Module, policy: &Policy, input: &Value) -> Result<ToolResult> {
        let mut store = Store::new(engine, HostState::default());
        let linker = build_linker(engine, policy)?;
        let instance = linker
            .instantiate_and_start(&mut store, module)
            .map_err(|e| SeppError::Tool(format!("wasm instantiate: {e}")))?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| SeppError::Tool("wasm: kein 'memory'-Export".into()))?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&store, "sepp_alloc")
            .map_err(|e| SeppError::Tool(format!("wasm: sepp_alloc fehlt: {e}")))?;
        let call = instance
            .get_typed_func::<(i32, i32), i64>(&store, "sepp_call")
            .map_err(|e| SeppError::Tool(format!("wasm: sepp_call fehlt: {e}")))?;

        let input_bytes = serde_json::to_vec(input)
            .map_err(|e| SeppError::Tool(format!("wasm input-json: {e}")))?;
        let len = input_bytes.len() as i32;
        let ptr = alloc
            .call(&mut store, len)
            .map_err(|e| SeppError::Tool(format!("wasm sepp_alloc: {e}")))?;
        memory
            .write(&mut store, ptr as usize, &input_bytes)
            .map_err(|e| SeppError::Tool(format!("wasm write input: {e}")))?;

        let packed = call
            .call(&mut store, (ptr, len))
            .map_err(|e| SeppError::Tool(format!("wasm sepp_call: {e}")))?;
        let (rptr, rlen) = unpack(packed);
        let out = read_mem(&memory, &store, rptr, rlen)?;
        let mut result: ToolResult = serde_json::from_slice(&out)
            .map_err(|e| SeppError::Tool(format!("wasm result-json: {e}")))?;
        // Tool-Output IMMER kürzen, bevor er ins Kontextfenster geht (Plugin kürzt nicht selbst).
        result.content = sepp_tools::truncate_content_blocks(result.content);
        Ok(result)
    }
}

#[async_trait]
impl Tool for WasmPlugin {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn execute(
        &self,
        input: Value,
        cancel: CancellationToken,
        _on_update: Option<&(dyn Fn(ToolResult) + Send + Sync)>,
    ) -> Result<ToolResult> {
        // Cancel wird vor dem Start geprüft (mid-call-Abbruch via fuel ist eine Folgearbeit).
        if cancel.is_cancelled() {
            return Err(SeppError::Aborted);
        }
        // wasmi ist synchron → in den Blocking-Pool auslagern, damit der (Single-Thread-)Reactor
        // frei bleibt und parallele Tool-Calls nebenläufig laufen können.
        let engine = self.engine.clone();
        let module = self.module.clone();
        let policy = self.policy.clone();
        tokio::task::spawn_blocking(move || WasmPlugin::run(&engine, &module, &policy, &input))
            .await
            .map_err(|e| SeppError::Tool(format!("wasm task: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"{"name":"compute","label":"Compute","description":"WAT-Test-Plugin","parameters":{"type":"object"}}"#;

    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }

    /// Plugin, das die Eingabe in den Ergebnis-Text einbettet (`in:<input>`) — beweist, dass
    /// Daten rein UND raus fließen. Nutzt `host_log` (immer verfügbar). Eingabe im Test ist
    /// quote-frei, damit das Resultat gültiges JSON bleibt.
    fn compute_wat() -> Vec<u8> {
        let prefix = r#"{"content":[{"type":"text","text":"in:"#;
        let suffix = r#""}]}"#;
        let wat = format!(
            r#"(module
  (import "env" "host_log" (func $log (param i32 i32)))
  (memory (export "memory") 2)
  (data (i32.const 8) "{spec}")
  (data (i32.const 4096) "{prefix}")
  (data (i32.const 5096) "{suffix}")
  (global $bump (mut i32) (i32.const 8192))
  (func (export "sepp_alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $n)))
    (local.get $p))
  (func (export "sepp_spec") (result i64)
    (i64.or (i64.shl (i64.const 8) (i64.const 32)) (i64.const {speclen})))
  (func (export "sepp_call") (param $ptr i32) (param $len i32) (result i64)
    (local $out i32) (local $cur i32) (local $total i32)
    (call $log (local.get $ptr) (local.get $len))
    (local.set $out (i32.const 65536))
    (memory.copy (local.get $out) (i32.const 4096) (i32.const {plen}))
    (local.set $cur (i32.add (local.get $out) (i32.const {plen})))
    (memory.copy (local.get $cur) (local.get $ptr) (local.get $len))
    (local.set $cur (i32.add (local.get $cur) (local.get $len)))
    (memory.copy (local.get $cur) (i32.const 5096) (i32.const {slen}))
    (local.set $total (i32.add (i32.add (i32.const {plen}) (local.get $len)) (i32.const {slen})))
    (i64.or (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
            (i64.extend_i32_u (local.get $total))))
)"#,
            spec = esc(SPEC),
            prefix = esc(prefix),
            suffix = esc(suffix),
            speclen = SPEC.len(),
            plen = prefix.len(),
            slen = suffix.len(),
        );
        wat::parse_str(&wat).expect("compute wat")
    }

    /// Plugin, das `host_http` importiert → ohne Net-Capability nicht instanziierbar.
    fn net_wat() -> Vec<u8> {
        let spec =
            r#"{"name":"netter","label":"Net","description":"x","parameters":{"type":"object"}}"#;
        let wat = format!(
            r#"(module
  (import "env" "host_http" (func $http (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 8) "{spec}")
  (global $bump (mut i32) (i32.const 1024))
  (func (export "sepp_alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $n)))
    (local.get $p))
  (func (export "sepp_spec") (result i64)
    (i64.or (i64.shl (i64.const 8) (i64.const 32)) (i64.const {speclen})))
  (func (export "sepp_call") (param i32) (param i32) (result i64)
    (i64.const 0))
)"#,
            spec = esc(spec),
            speclen = spec.len(),
        );
        wat::parse_str(&wat).expect("net wat")
    }

    #[tokio::test]
    async fn loads_and_runs_plugin_as_tool() {
        let host = WasmHost::new();
        let plugin = host.load(&compute_wat(), Policy::default()).unwrap();
        assert_eq!(plugin.spec().name, "compute");

        let r = plugin
            .execute(serde_json::json!({}), CancellationToken::new(), None)
            .await
            .unwrap();
        assert!(!r.is_error);
        // Eingabe `{}` floss durch das Plugin in den Ergebnis-Text.
        assert!(matches!(&r.content[0],
            sepp_core::ContentBlock::Text { text } if text == "in:{}"));
    }

    #[test]
    fn net_plugin_blocked_without_net_capability() {
        let host = WasmHost::new();
        // Ohne Net: host_http wird nicht registriert → Instanziierung scheitert.
        let denied = host.load(&net_wat(), Policy::default());
        assert!(
            denied.is_err(),
            "Plugin ohne Net-Capability durfte NICHT laden"
        );

        // Mit Net: host_http registriert → lädt.
        let granted = host.load(
            &net_wat(),
            Policy::new(vec![Capability::Net {
                host: "example.com".into(),
            }]),
        );
        assert!(granted.is_ok(), "Plugin mit Net-Capability sollte laden");
    }
}
