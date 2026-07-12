//! `sepp-wasm` — Tier-2-Plugin-Host: lädt WASM-Plugins als Tools, **capability-gated**.
//!
//! Sicherheit: WASM ist von Natur aus capability-secure — ein Modul kann nur, was der Host als
//! Funktion bereitstellt. Host-Funktionen werden **nur registriert, wenn die Policy sie erlaubt**.
//! Ein Plugin ohne `Net`-Capability importiert `host_http`, das
//! dann nicht existiert → Instanziierung schlägt fehl → es kann nachweislich nicht ins Netz.
//!
//! Neben *Zugriff* (Capabilities) ist auch *Verbrauch* gedeckelt ([`Limits`] aus dem
//! `[limits]`-Manifest-Abschnitt, fehlend = konservative Defaults): CPU über **Fuel-Slicing**
//! (die Ausführung yieldet nach `fuel_slice` Instruktionen an den Host), Speicher über ein
//! hartes Page-Limit (`memory.grow` darüber liefert dem Plugin regulär `-1`), Laufzeit über
//! ein Wanduhr-Budget. Fuel ist dabei kein Todesurteil, sondern ein **Yield-Punkt**: bei
//! leerem Tank entscheidet der Host (abgebrochen? Zeit um?), tankt nach und setzt die
//! Ausführung im erhaltenen Zustand fort (`call_resumable`, kein Neustart).
//! `max_wall_time_ms = 0` heißt beliebig lange laufen dürfen — niemals unkontrollierbar sein.
//!
//! Plugin-ABI (Exports): `sepp_alloc(i32)->i32`, `sepp_spec()->i64`, `sepp_call(i32,i32)->i64`.
//! Der Rückgabewert `i64` packt `(ptr<<32 | len)`. `sepp_spec` liefert ToolSpec-JSON, `sepp_call`
//! erhält die Argument-JSON und liefert ToolResult-JSON (beides im linearen Speicher).
//! Gegatete Host-Importe (`env`-Modul): `host_log(i32,i32)` (immer), `host_fs_read`/`host_http`
//! (nur bei passender Capability).

use std::path::Path;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use wasmi::{
    Caller, Config, Engine, Extern, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder,
    TypedFunc, TypedResumableCall, WasmParams, WasmResults,
};

use sepp_core::{Result, SeppError, ToolResult, ToolSpec};
use sepp_policy::{Capability, Limits, Manifest, Policy};
use sepp_tools::Tool;

/// Pro-Instanz-Zustand (für Host-Funktionen und den Speicher-Limiter).
struct HostState {
    logs: Vec<String>,
    /// Speicher-Deckel dieses Stores: `memory.grow` über dem Limit liefert dem Plugin `-1`
    /// (regulär, kein Trap) — Host-RAM bleibt flach, egal was das Plugin versucht.
    limits: StoreLimits,
}

fn host_state(limits: &Limits) -> HostState {
    HostState {
        logs: Vec::new(),
        limits: StoreLimitsBuilder::new()
            .memory_size(limits.max_memory_bytes())
            .build(),
    }
}

/// Obergrenze für Plugin-Rückgaben (ToolSpec-/ToolResult-JSON), damit ein bösartiges Plugin
/// den Host nicht durch eine riesige `len` zu einer GB-Allokation zwingt.
const MAX_PLUGIN_BYTES: u32 = 16 * 1024 * 1024;

/// Fuel-Tank für `instantiate_and_start`: die Start-Sektion ist nicht resumierbar und bekommt
/// deshalb ein festes, großzügiges Einmal-Budget statt Slicing — fail-closed bei Überschreitung.
const START_FUEL: u64 = 10_000_000;

/// Wanduhr-Deckel für den Lade-Pfad (Instanziierung + `sepp_spec` beim Discovery): beim Start
/// gibt es keinen Abbruchkanal, also gilt hier IMMER ein hartes Budget — auch wenn das Manifest
/// für Tool-Calls `max_wall_time_ms = 0` (unbegrenzt) erlaubt.
const LOAD_WALL_MS: u64 = 5_000;

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

/// Kooperatives CPU-Budget für Plugin-Aufrufe.
///
/// Jeder Export-Aufruf läuft in Fuel-Scheiben: nach `fuel_slice` Instruktionen kommt die
/// Kontrolle zum Host zurück (Yield-Punkt), der Abbruch und Wanduhr prüft, nachtankt und die
/// Ausführung **im erhaltenen Zustand** fortsetzt. Die Wanduhr läuft über alle Aufrufe eines
/// Budgets hinweg (ein Tool-Call = ein Budget, egal wie viele Exports er berührt).
struct FuelBudget<'a> {
    limits: &'a Limits,
    cancel: &'a CancellationToken,
    started: Instant,
    /// Zuletzt getankter Betrag (für die Verbrauchsrechnung am nächsten Yield-Punkt).
    last_tank: u64,
    /// Insgesamt verbranntes Fuel (Fehler-Kontext und Fortschrittsmeldung).
    total_fuel: u64,
}

impl<'a> FuelBudget<'a> {
    fn new(limits: &'a Limits, cancel: &'a CancellationToken) -> Self {
        FuelBudget {
            limits,
            cancel,
            started: Instant::now(),
            last_tank: 0,
            total_fuel: 0,
        }
    }

    fn refuel(&mut self, store: &mut Store<HostState>, amount: u64) -> Result<()> {
        self.last_tank = amount;
        store
            .set_fuel(amount)
            .map_err(|e| SeppError::Tool(format!("wasm fuel: {e}")))
    }

    /// Verbucht das seit dem letzten Tanken verbrannte Fuel.
    fn settle(&mut self, store: &Store<HostState>) {
        let remaining = store.get_fuel().unwrap_or(0);
        self.total_fuel = self
            .total_fuel
            .saturating_add(self.last_tank.saturating_sub(remaining));
    }

    /// Führt einen Plugin-Export unter dem Budget aus (Refuel-Loop statt Ein-Weg-Aufruf).
    fn call<P, R>(
        &mut self,
        store: &mut Store<HostState>,
        func: &TypedFunc<P, R>,
        params: P,
        what: &str,
    ) -> Result<R>
    where
        P: WasmParams,
        R: WasmResults,
    {
        self.refuel(store, self.limits.fuel_slice)?;
        let mut state = func
            .call_resumable(&mut *store, params)
            .map_err(|e| SeppError::Tool(format!("wasm {what}: {e}")))?;
        loop {
            match state {
                TypedResumableCall::Finished(v) => {
                    self.settle(store);
                    return Ok(v);
                }
                TypedResumableCall::OutOfFuel(invocation) => {
                    // Kontrolle ist zurück beim Host. Nur hier wird entschieden.
                    self.settle(store);
                    if self.cancel.is_cancelled() {
                        return Err(SeppError::Aborted);
                    }
                    let elapsed_ms = self.started.elapsed().as_millis() as u64;
                    if self.limits.max_wall_time_ms > 0 && elapsed_ms > self.limits.max_wall_time_ms
                    {
                        return Err(SeppError::Tool(format!(
                            "wasm {what}: Zeitbudget überschritten \
                             ({} ms Limit, {elapsed_ms} ms gelaufen, {} Fuel verbraucht)",
                            self.limits.max_wall_time_ms, self.total_fuel
                        )));
                    }
                    // Fortschritts-Hook: hier kann später der TUI-Status-Kanal andocken.
                    tracing::trace!(
                        target: "wasm",
                        "{what}: yield nach {} Fuel, {elapsed_ms} ms",
                        self.total_fuel
                    );
                    // Mindestens `required_fuel` tanken, sonst käme eine Operation, die mehr
                    // als eine ganze Scheibe kostet, nie voran (Yield-Endlosschleife).
                    let tank = self.limits.fuel_slice.max(invocation.required_fuel());
                    self.refuel(store, tank)?;
                    state = invocation
                        .resume(&mut *store)
                        .map_err(|e| SeppError::Tool(format!("wasm {what}: {e}")))?;
                }
                TypedResumableCall::HostTrap(_) => {
                    // Unsere Host-Funktionen liefern keine Fehler — defensiv abfangen.
                    return Err(SeppError::Tool(format!(
                        "wasm {what}: unerwarteter Host-Trap"
                    )));
                }
            }
        }
    }
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

/// Der WASM-Host (hält die `wasmi`-Engine, Fuel-Metering aktiv).
pub struct WasmHost {
    engine: Engine,
}

impl Default for WasmHost {
    fn default() -> Self {
        Self::new()
    }
}

impl WasmHost {
    pub fn new() -> Self {
        // Fuel-Metering engine-weit: JEDE Plugin-Ausführung ist damit unterbrechbar.
        let mut config = Config::default();
        config.consume_fuel(true);
        WasmHost {
            engine: Engine::new(&config),
        }
    }

    /// Lädt ein Plugin aus WASM-Bytes mit Policy und Limits. Instanziiert einmal, um die
    /// `ToolSpec` zu holen (scheitert, wenn Capability-gegatete Importe fehlen → Gate).
    /// Auch dieser Lade-Pfad läuft unter Budget: ein Plugin, das schon in der Start-Sektion
    /// oder in `sepp_spec` endlos rechnet, kann den Sepp-Start nicht aufhängen.
    pub fn load(&self, wasm: &[u8], policy: Policy, limits: Limits) -> Result<WasmPlugin> {
        let module = Module::new(&self.engine, wasm)
            .map_err(|e| SeppError::Tool(format!("wasm compile: {e}")))?;

        let mut store = Store::new(&self.engine, host_state(&limits));
        store.limiter(|state| &mut state.limits);
        let linker = build_linker(&self.engine, &policy)?;
        store
            .set_fuel(START_FUEL.max(limits.fuel_slice))
            .map_err(|e| SeppError::Tool(format!("wasm fuel: {e}")))?;
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|e| SeppError::Tool(format!("wasm instantiate: {e}")))?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| SeppError::Tool("wasm: kein 'memory'-Export".into()))?;
        let spec_fn = instance
            .get_typed_func::<(), i64>(&store, "sepp_spec")
            .map_err(|e| SeppError::Tool(format!("wasm: sepp_spec fehlt: {e}")))?;

        // Beim Laden gibt es keinen Abbruchkanal → hartes Wanduhr-Budget, „unbegrenzt" zählt
        // hier nicht.
        let mut load_limits = limits.clone();
        load_limits.max_wall_time_ms = match load_limits.max_wall_time_ms {
            0 => LOAD_WALL_MS,
            ms => ms.min(LOAD_WALL_MS),
        };
        let never = CancellationToken::new();
        let mut budget = FuelBudget::new(&load_limits, &never);
        let packed = budget.call(&mut store, &spec_fn, (), "sepp_spec")?;
        let (ptr, len) = unpack(packed);
        let bytes = read_mem(&memory, &store, ptr, len)?;
        let spec: ToolSpec = serde_json::from_slice(&bytes)
            .map_err(|e| SeppError::Tool(format!("wasm spec-json: {e}")))?;

        Ok(WasmPlugin {
            engine: self.engine.clone(),
            module,
            policy,
            limits,
            spec,
        })
    }

    /// Lädt ein Plugin aus einer Datei; Capabilities und Limits aus dem (optionalen) Manifest.
    pub fn load_file(&self, wasm_path: &Path, manifest_path: Option<&Path>) -> Result<WasmPlugin> {
        let wasm = std::fs::read(wasm_path)
            .map_err(|e| SeppError::Tool(format!("wasm read {}: {e}", wasm_path.display())))?;
        let (policy, limits) = match manifest_path {
            Some(p) => {
                let manifest = Manifest::from_file(p)?;
                (manifest.policy(), manifest.limits.clone())
            }
            None => (Policy::default(), Limits::default()),
        };
        self.load(&wasm, policy, limits)
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
    limits: Limits,
    spec: ToolSpec,
}

impl WasmPlugin {
    /// Überschreibt den exponierten Tool-Namen (für Kollisions-Präfixe im gemeinsamen Toolset).
    pub fn rename(&mut self, name: String) {
        self.spec.label = name.clone();
        self.spec.name = name;
    }

    /// Synchroner Plugin-Lauf unter [`FuelBudget`]. Assoziierte Funktion (kein `&self`), damit
    /// `execute` sie per `spawn_blocking` in den Blocking-Pool auslagern kann (der Reactor
    /// bleibt frei). Das `cancel`-Token wird an jedem Yield-Punkt geprüft — ein rechnendes
    /// Plugin bricht binnen einer Fuel-Scheibe ab.
    fn run(
        engine: &Engine,
        module: &Module,
        policy: &Policy,
        limits: &Limits,
        input: &Value,
        cancel: &CancellationToken,
    ) -> Result<ToolResult> {
        let mut store = Store::new(engine, host_state(limits));
        store.limiter(|state| &mut state.limits);
        let linker = build_linker(engine, policy)?;
        let mut budget = FuelBudget::new(limits, cancel);
        store
            .set_fuel(START_FUEL.max(limits.fuel_slice))
            .map_err(|e| SeppError::Tool(format!("wasm fuel: {e}")))?;
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
        let ptr = budget.call(&mut store, &alloc, len, "sepp_alloc")?;
        memory
            .write(&mut store, ptr as usize, &input_bytes)
            .map_err(|e| SeppError::Tool(format!("wasm write input: {e}")))?;

        let packed = budget.call(&mut store, &call, (ptr, len), "sepp_call")?;
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
        if cancel.is_cancelled() {
            return Err(SeppError::Aborted);
        }
        // wasmi ist synchron → in den Blocking-Pool auslagern, damit der (Single-Thread-)Reactor
        // frei bleibt und parallele Tool-Calls nebenläufig laufen. Das Token wandert mit hinein:
        // der Refuel-Loop prüft es an jedem Yield-Punkt (Mid-Call-Abbruch via Fuel).
        let engine = self.engine.clone();
        let module = self.module.clone();
        let policy = self.policy.clone();
        let limits = self.limits.clone();
        tokio::task::spawn_blocking(move || {
            WasmPlugin::run(&engine, &module, &policy, &limits, &input, &cancel)
        })
        .await
        .map_err(|e| SeppError::Tool(format!("wasm task: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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

    /// Baut ein minimales Plugin (Standard-Exports) mit eigenem `sepp_call`-Rumpf.
    fn plugin_wat(spec: &str, mem_pages: u32, extra_data: &str, sepp_call: &str) -> Vec<u8> {
        let wat = format!(
            r#"(module
  (memory (export "memory") {mem_pages})
  (data (i32.const 8) "{spec_esc}")
  {extra_data}
  (global $bump (mut i32) (i32.const 1024))
  (func (export "sepp_alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $n)))
    (local.get $p))
  (func (export "sepp_spec") (result i64)
    (i64.or (i64.shl (i64.const 8) (i64.const 32)) (i64.const {speclen})))
  {sepp_call}
)"#,
            spec_esc = esc(spec),
            speclen = spec.len(),
        );
        wat::parse_str(&wat).expect("plugin wat")
    }

    /// Endlosschleife in `sepp_call` (`loop br 0`) — terminiert nie von selbst.
    fn spin_wat() -> Vec<u8> {
        let spec =
            r#"{"name":"spin","label":"Spin","description":"x","parameters":{"type":"object"}}"#;
        plugin_wat(
            spec,
            1,
            "",
            r#"(func (export "sepp_call") (param i32) (param i32) (result i64)
    (loop $l (br $l))
    (i64.const 0))"#,
        )
    }

    /// Zählt bis `n` und liefert nur bei korrektem Endstand das Ergebnis — beweist, dass der
    /// Zustand (Locals) über viele Yield-Punkte erhalten bleibt. Ein Neustart-Bug statt
    /// `resume` käme hier nie durch.
    fn count_wat(n: u32) -> Vec<u8> {
        let spec =
            r#"{"name":"count","label":"Count","description":"x","parameters":{"type":"object"}}"#;
        let ok = r#"{"content":[{"type":"text","text":"done"}]}"#;
        let call = format!(
            r#"(func (export "sepp_call") (param i32) (param i32) (result i64)
    (local $i i32)
    (loop $l
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_u (local.get $i) (i32.const {n}))))
    (if (i32.ne (local.get $i) (i32.const {n})) (then (unreachable)))
    (i64.or (i64.shl (i64.const 4096) (i64.const 32)) (i64.const {oklen})))"#,
            oklen = ok.len(),
        );
        let data = format!(r#"(data (i32.const 4096) "{}")"#, esc(ok));
        plugin_wat(spec, 1, &data, &call)
    }

    /// Versucht `memory.grow` um `pages` und meldet, ob der Host das erlaubt hat.
    fn grow_wat(pages: u32) -> Vec<u8> {
        let spec =
            r#"{"name":"grower","label":"Grow","description":"x","parameters":{"type":"object"}}"#;
        let denied = r#"{"content":[{"type":"text","text":"grow-denied"}]}"#;
        let allowed = r#"{"content":[{"type":"text","text":"grow-allowed"}]}"#;
        let call = format!(
            r#"(func (export "sepp_call") (param i32) (param i32) (result i64)
    (if (result i64) (i32.eq (memory.grow (i32.const {pages})) (i32.const -1))
      (then (i64.or (i64.shl (i64.const 4096) (i64.const 32)) (i64.const {dlen})))
      (else (i64.or (i64.shl (i64.const 5120) (i64.const 32)) (i64.const {alen})))))"#,
            dlen = denied.len(),
            alen = allowed.len(),
        );
        let data = format!(
            r#"(data (i32.const 4096) "{}")
  (data (i32.const 5120) "{}")"#,
            esc(denied),
            esc(allowed)
        );
        plugin_wat(spec, 2, &data, &call)
    }

    fn text_of(r: &ToolResult) -> &str {
        match &r.content[0] {
            sepp_core::ContentBlock::Text { text } => text,
            other => panic!("Text erwartet, war: {other:?}"),
        }
    }

    #[tokio::test]
    async fn loads_and_runs_plugin_as_tool() {
        let host = WasmHost::new();
        let plugin = host
            .load(&compute_wat(), Policy::default(), Limits::default())
            .unwrap();
        assert_eq!(plugin.spec().name, "compute");

        let r = plugin
            .execute(serde_json::json!({}), CancellationToken::new(), None)
            .await
            .unwrap();
        assert!(!r.is_error);
        // Eingabe `{}` floss durch das Plugin in den Ergebnis-Text.
        assert_eq!(text_of(&r), "in:{}");
    }

    #[test]
    fn net_plugin_blocked_without_net_capability() {
        let host = WasmHost::new();
        // Ohne Net: host_http wird nicht registriert → Instanziierung scheitert.
        let denied = host.load(&net_wat(), Policy::default(), Limits::default());
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
            Limits::default(),
        );
        assert!(granted.is_ok(), "Plugin mit Net-Capability sollte laden");
    }

    /// Spec-Test 1: Endlosschleife wird unterbrochen, nicht getötet — der Refuel-Loop tankt
    /// nach, bis die Wanduhr greift; der Host lebt weiter.
    #[tokio::test]
    async fn endless_loop_hits_wall_clock_budget() {
        let host = WasmHost::new();
        let limits = Limits {
            max_wall_time_ms: 100,
            fuel_slice: 100_000,
            ..Limits::default()
        };
        let plugin = host.load(&spin_wat(), Policy::default(), limits).unwrap();

        let res = tokio::time::timeout(
            Duration::from_secs(30),
            plugin.execute(serde_json::json!({}), CancellationToken::new(), None),
        )
        .await
        .expect("Wanduhr-Budget muss den Lauf beenden");
        let err = res.expect_err("Zeitbudget-Fehler erwartet");
        assert!(err.to_string().contains("Zeitbudget"), "war: {err}");
    }

    /// Spec-Tests 2 + 5: Abbruch wirkt — auch bei `max_wall_time_ms = 0` (unbegrenzt heißt
    /// lange laufen dürfen, nicht unkontrollierbar sein).
    #[tokio::test]
    async fn cancel_interrupts_endless_plugin_even_with_unlimited_wall_time() {
        let host = WasmHost::new();
        let limits = Limits {
            max_wall_time_ms: 0,
            ..Limits::default()
        };
        let plugin = host.load(&spin_wat(), Policy::default(), limits).unwrap();

        let cancel = CancellationToken::new();
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            c.cancel();
        });
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            plugin.execute(serde_json::json!({}), cancel, None),
        )
        .await
        .expect("Abbruch muss binnen Sekunden wirken");
        assert!(matches!(res, Err(SeppError::Aborted)), "war: {res:?}");
    }

    /// Spec-Test 3: Lange, aber legitime Rechnung läuft durch — Nachtanken funktioniert und
    /// der Zustand bleibt über viele Yield-Punkte erhalten.
    #[tokio::test]
    async fn long_computation_survives_many_yield_points() {
        let host = WasmHost::new();
        // 500k Iterationen bei 20k-Fuel-Scheiben → viele Nachtank-Zyklen nötig.
        let limits = Limits {
            fuel_slice: 20_000,
            max_wall_time_ms: 10_000,
            ..Limits::default()
        };
        let plugin = host
            .load(&count_wat(500_000), Policy::default(), limits)
            .unwrap();

        let r = plugin
            .execute(serde_json::json!({}), CancellationToken::new(), None)
            .await
            .unwrap();
        assert!(!r.is_error);
        assert_eq!(text_of(&r), "done");
    }

    /// Spec-Test 4: Speicherlimit greift — `memory.grow` über dem Limit liefert dem Plugin
    /// `-1` (regulär), statt Host-RAM zu belegen.
    #[tokio::test]
    async fn memory_grow_beyond_limit_returns_minus_one() {
        let host = WasmHost::new();
        let limits = Limits {
            max_memory_pages: 4,
            ..Limits::default()
        };
        let plugin = host
            .load(&grow_wat(1024), Policy::default(), limits)
            .unwrap();

        let r = plugin
            .execute(serde_json::json!({}), CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(text_of(&r), "grow-denied");
    }

    /// Gegenprobe zu Test 4: ein Grow innerhalb des Limits (2+2 ≤ 8 Pages) bleibt erlaubt —
    /// der Deckel greift exakt am Manifest-Limit, nicht pauschal.
    #[tokio::test]
    async fn memory_grow_within_limit_is_allowed() {
        let host = WasmHost::new();
        let limits = Limits {
            max_memory_pages: 8,
            ..Limits::default()
        };
        let plugin = host.load(&grow_wat(2), Policy::default(), limits).unwrap();

        let r = plugin
            .execute(serde_json::json!({}), CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(text_of(&r), "grow-allowed");
    }

    /// Spec-Test 6: Ein rechnendes Plugin blockiert den Reactor nicht — parallele Arbeit
    /// läuft weiter, und der Abbruch wirkt von außen.
    #[tokio::test]
    async fn computing_plugin_does_not_block_the_reactor() {
        let host = WasmHost::new();
        let limits = Limits {
            max_wall_time_ms: 0,
            ..Limits::default()
        };
        let plugin = host.load(&spin_wat(), Policy::default(), limits).unwrap();

        let cancel = CancellationToken::new();
        let c = cancel.clone();
        let task =
            tokio::spawn(async move { plugin.execute(serde_json::json!({}), c, None).await });

        // Während das Plugin im Blocking-Pool rechnet, muss der Reactor frei sein:
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!task.is_finished(), "Plugin sollte noch rechnen (wall=0)");

        cancel.cancel();
        let res = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("Abbruch muss wirken")
            .expect("join");
        assert!(matches!(res, Err(SeppError::Aborted)), "war: {res:?}");
    }
}
