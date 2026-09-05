//! `sepp-wasm` — Tier-2-Plugin-Host: lädt WASM-Plugins als Tools, **capability-gated**.
//!
//! Sicherheit: WASM ist von Natur aus capability-secure — ein Modul kann nur, was der Host als
//! Funktion bereitstellt. Host-Funktionen werden **nur registriert, wenn die Policy sie erlaubt**.
//! Ein Plugin ohne `Net`-Capability importiert `host_http`, das
//! dann nicht existiert → Instanziierung schlägt fehl → es kann nachweislich nicht ins Netz.
//!
//! Die Policy ist der **Schnitt** aus der Manifest-Anfrage und der Gewährung in
//! `policy.toml [plugin.<name>]`. Ohne Gewährung ist der Schnitt **leer**: Das Manifest liegt
//! neben der wasm-Datei und stammt vom Autor des Plugins, es ist eine Selbstauskunft und ohne
//! Gegenzeichnung keine Grenze. Ein Plugin, dessen Manifest etwas fordert, das niemand gewährt
//! hat, lädt deshalb gar nicht.
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
//! **Plugin-ABI Version 1** ([`PLUGIN_ABI`]); ein Manifest deklariert sie über `abi`, ein
//! höherer Wert wird abgelehnt.
//!
//! Exports, **alle vier beim Laden geprüft**: `sepp_alloc(i32)->i32`, `sepp_spec()->i64`,
//! `sepp_call(i32,i32)->i64` und die Memory unter dem Namen `memory`. Der Rückgabewert `i64`
//! packt `(ptr<<32 | len)`. `sepp_spec` liefert ToolSpec-JSON, `sepp_call` erhält die
//! Argument-JSON und liefert ToolResult-JSON (beides im linearen Speicher).
//!
//! Importe aus dem Modul `env`: `host_log(i32,i32)` und `host_result_read(i32,i32)->i32` immer,
//! `host_fs_read(i32,i32)->i32` mit `FsRead`, `host_http(i32,i32)->i32` mit `Net`.
//!
//! **Der Abholweg:** Eine Fähigkeit führt aus, legt ihr Ergebnis im Host ab und meldet dessen
//! Größe; `host_result_read` kopiert es in einen Puffer, den das Plugin passend dimensioniert
//! hat. Damit wird nie doppelt gesendet und niemand muss eine Größe raten. Die Alternative
//! wäre, dass der Host aus der Host-Funktion heraus `sepp_alloc` aufruft — dieser Rücksprung
//! läuft nicht resumierbar und kollidiert mit dem Fuel-Slicing. Eine Fähigkeit liefert immer
//! ein JSON-Objekt, auch im Fehlerfall (`{"error":"…"}`), und trappt nie.

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
    /// Ergebnis der zuletzt aufgerufenen Fähigkeit, abholbar über `host_result_read`.
    ///
    /// Der Umweg über den Host ist Absicht: Die naheliegende Alternative wäre, dass der Host das
    /// Ergebnis selbst in den Modulspeicher schreibt und dafür `sepp_alloc` aufruft. Dieser
    /// Rücksprung läuft nicht resumierbar und kollidiert mit dem Fuel-Slicing. So stellt das
    /// Plugin den Puffer, und ein zu klein geratener zwingt nicht zum erneuten Senden.
    result: Vec<u8>,
    /// Effektive Rechte dieses Plugins — die Host-Funktionen prüfen damit Pfade und Hosts.
    policy: Policy,
}

fn host_state(limits: &Limits, policy: Policy) -> HostState {
    HostState {
        logs: Vec::new(),
        limits: StoreLimitsBuilder::new()
            .memory_size(limits.max_memory_bytes())
            .build(),
        result: Vec::new(),
        policy,
    }
}

/// Legt ein Ergebnis für `host_result_read` bereit und liefert dessen Größe zurück.
/// Fähigkeiten geben **immer** ein JSON-Objekt zurück, auch im Fehlerfall — ein Plugin soll
/// eine Erklärung bekommen, keinen Absturz.
fn stage(caller: &mut Caller<'_, HostState>, json: serde_json::Value) -> i32 {
    let bytes = json.to_string().into_bytes();
    let n = bytes.len();
    caller.data_mut().result = bytes;
    n as i32
}

/// Fehler-Ergebnis einer Fähigkeit.
fn stage_err(caller: &mut Caller<'_, HostState>, msg: impl std::fmt::Display) -> i32 {
    stage(caller, serde_json::json!({ "error": msg.to_string() }))
}

/// Liest die Eingabe einer Fähigkeit aus dem Modulspeicher.
fn read_input(caller: &Caller<'_, HostState>, mem: &Memory, ptr: i32, len: i32) -> Option<Vec<u8>> {
    let (a, b) = (
        ptr.max(0) as usize,
        ptr.max(0) as usize + len.max(0) as usize,
    );
    mem.data(caller).get(a..b).map(<[u8]>::to_vec)
}

/// Obergrenze für Nutzdaten in **beide** Richtungen: für Plugin-Rückgaben (ToolSpec-/
/// ToolResult-JSON), damit ein bösartiges Plugin den Host nicht durch eine riesige `len` zu einer
/// GB-Allokation zwingt, und für Ergebnisse der Host-Fähigkeiten, damit eine große Datei oder
/// Antwort nicht den Speicher des Moduls sprengt.
const MAX_PLUGIN_BYTES: u32 = 16 * 1024 * 1024;

/// Version des Plugin-Protokolls, die dieser Host spricht. Ein Manifest mit höherer Angabe wird
/// abgelehnt — lieber gar nicht laden als mit falschen Erwartungen laufen.
pub const PLUGIN_ABI: u32 = 1;

/// Fuel-Tank für `instantiate_and_start`: die Start-Sektion ist nicht resumierbar und bekommt
/// deshalb ein festes, großzügiges Einmal-Budget statt Slicing — fail-closed bei Überschreitung.
const START_FUEL: u64 = 10_000_000;

/// Wanduhr-Deckel für den Lade-Pfad (Instanziierung + `sepp_spec` beim Discovery): beim Start
/// gibt es keinen Abbruchkanal, also gilt hier IMMER ein hartes Budget — auch wenn das Manifest
/// für Tool-Calls `max_wall_time_ms = 0` (unbegrenzt) erlaubt.
const LOAD_WALL_MS: u64 = 5_000;

/// Prüft die vier Pflicht-Exports samt Signatur — ohne Store, ohne Instanziierung, direkt am
/// kompilierten Modul. Kostet kein Fuel und meldet den erwarteten Typ, damit ein Autor weiß,
/// was er falsch gemacht hat.
fn check_exports(module: &Module) -> Result<()> {
    use wasmi::{ExternType, ValType};

    let want: &[(&str, &[ValType], &[ValType])] = &[
        ("sepp_alloc", &[ValType::I32], &[ValType::I32]),
        ("sepp_spec", &[], &[ValType::I64]),
        ("sepp_call", &[ValType::I32, ValType::I32], &[ValType::I64]),
    ];
    for (name, params, results) in want {
        match module.get_export(name) {
            Some(ExternType::Func(ty)) if ty.params() == *params && ty.results() == *results => {}
            Some(ExternType::Func(ty)) => {
                return Err(SeppError::Tool(format!(
                    "wasm: Export `{name}` hat die falsche Signatur (({}) -> ({})), erwartet \
                     wird (({}) -> ({}))",
                    join_types(ty.params()),
                    join_types(ty.results()),
                    join_types(params),
                    join_types(results)
                )))
            }
            Some(_) => {
                return Err(SeppError::Tool(format!(
                    "wasm: `{name}` ist exportiert, aber keine Funktion"
                )))
            }
            None => {
                return Err(SeppError::Tool(format!(
                    "wasm: Export `{name}` fehlt (erwartet: ({}) -> ({}))",
                    join_types(params),
                    join_types(results)
                )))
            }
        }
    }
    match module.get_export("memory") {
        Some(ExternType::Memory(_)) => Ok(()),
        _ => Err(SeppError::Tool("wasm: kein 'memory'-Export".into())),
    }
}

fn join_types(ts: &[wasmi::ValType]) -> String {
    ts.iter()
        .map(|t| format!("{t:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

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

    // host_result_read: immer verfügbar. Holt das Ergebnis der zuletzt aufgerufenen Fähigkeit
    // in einen Puffer, den das Plugin selbst gestellt hat. Kopiert höchstens `cap` Bytes und
    // liefert die kopierte Zahl; ohne anliegendes Ergebnis null. Das Ergebnis bleibt erhalten,
    // bis die nächste Fähigkeit läuft — ein zu klein geratener Puffer zwingt also nicht dazu,
    // die Anfrage zu wiederholen. Trappt nie, auch nicht bei einem Ziel außerhalb des Speichers.
    linker
        .func_wrap(
            "env",
            "host_result_read",
            |mut caller: Caller<'_, HostState>, ptr: i32, cap: i32| -> i32 {
                let Some(Extern::Memory(mem)) = caller.get_export("memory") else {
                    return -1;
                };
                let take = caller.data().result.len().min(cap.max(0) as usize);
                if take == 0 {
                    return 0;
                }
                // Speicher und Zustand in einem Zug — zwei getrennte Zugriffe kollidierten
                // im Borrow-Checker.
                let (bytes, state) = mem.data_and_store_mut(&mut caller);
                let dst = ptr.max(0) as usize;
                let Some(slot) = bytes.get_mut(dst..dst + take) else {
                    return -1;
                };
                slot.copy_from_slice(&state.result[..take]);
                take as i32
            },
        )
        .map_err(|e| SeppError::Tool(format!("wasm linker host_result_read: {e}")))?;

    // host_fs_read: nur mit FsRead-Capability. Führt aus, legt das Ergebnis bereit und liefert
    // dessen Größe; abgeholt wird mit `host_result_read`.
    if policy
        .granted
        .iter()
        .any(|c| matches!(c, Capability::FsRead { .. }))
    {
        linker
            .func_wrap(
                "env",
                "host_fs_read",
                |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
                    let Some(Extern::Memory(mem)) = caller.get_export("memory") else {
                        return -1;
                    };
                    let Some(raw) = read_input(&caller, &mem, ptr, len) else {
                        return stage_err(&mut caller, "Eingabe liegt außerhalb des Speichers");
                    };
                    host_fs_read(&mut caller, &raw)
                },
            )
            .map_err(|e| SeppError::Tool(format!("wasm linker host_fs_read: {e}")))?;
    }
    // host_http: nur mit Net-Capability — DAS ist das Capability-Gate. Noch eine Attrappe, aber
    // eine ehrliche: Sie erklärt, statt eine Null zu liefern.
    if policy
        .granted
        .iter()
        .any(|c| matches!(c, Capability::Net { .. }))
    {
        linker
            .func_wrap(
                "env",
                "host_http",
                |mut caller: Caller<'_, HostState>, _ptr: i32, _len: i32| -> i32 {
                    stage_err(
                        &mut caller,
                        "host_http ist in dieser Version noch nicht implementiert",
                    )
                },
            )
            .map_err(|e| SeppError::Tool(format!("wasm linker host_http: {e}")))?;
    }

    Ok(linker)
}

/// Rumpf von `host_fs_read`: `{"path":"…"}` hinein, `{"bytes":N,"text":"…","lossy":false}`
/// hinaus, im Fehlerfall `{"error":"…"}`.
///
/// Der Pfad wird kanonisch aufgelöst und gegen dieselbe Policy geprüft, die auch `read`,
/// `write` und `edit` benutzen — ein Plugin kommt also nicht weiter als der Agent selbst.
/// Ein Fehler wird nie zum Trap: Das Modell kann mit einer Erklärung etwas anfangen, mit
/// einem abgestürzten Werkzeug nicht.
fn host_fs_read(caller: &mut Caller<'_, HostState>, input: &[u8]) -> i32 {
    let args: Value = match serde_json::from_slice(input) {
        Ok(v) => v,
        Err(e) => return stage_err(caller, format!("host_fs_read: ungültige Eingabe: {e}")),
    };
    let Some(raw_path) = args.get("path").and_then(Value::as_str) else {
        return stage_err(caller, "host_fs_read: Feld 'path' fehlt");
    };
    let ctx = sepp_policy::ResolveCtx::from_env();
    let path = sepp_policy::canonicalize_lenient(&sepp_policy::resolve_path_with(raw_path, &ctx));
    if !caller.data().policy.allows_path(&path, false) {
        return stage_err(
            caller,
            format!(
                "host_fs_read: {} liegt außerhalb der Rechte dieses Plugins",
                path.display()
            ),
        );
    }
    // Erst die Größe prüfen, dann lesen — sonst zöge eine riesige Datei den Host in eine
    // Allokation, die das Modul ohnehin nicht abholen könnte.
    match std::fs::metadata(&path) {
        Ok(m) if m.len() > MAX_PLUGIN_BYTES as u64 => {
            return stage_err(
                caller,
                format!(
                    "host_fs_read: {} ist zu groß ({} > {MAX_PLUGIN_BYTES} Bytes)",
                    path.display(),
                    m.len()
                ),
            )
        }
        Ok(_) => {}
        Err(e) => return stage_err(caller, format!("host_fs_read: {}: {e}", path.display())),
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return stage_err(caller, format!("host_fs_read: {}: {e}", path.display())),
    };
    let text = String::from_utf8_lossy(&bytes);
    let lossy = text.len() != bytes.len();
    stage(
        caller,
        serde_json::json!({ "bytes": bytes.len(), "text": text, "lossy": lossy }),
    )
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
        // Alle vier Exports schon hier prüfen, ohne Store und ohne Instanziierung. Vorher fehlten
        // `sepp_alloc` und `sepp_call` erst beim ersten Werkzeug-Aufruf auf — ein Plugin lud
        // scheinbar sauber und fiel später um. Für ein Paket, das jemand installiert, ist das
        // untragbar: kaputt muss beim Laden sichtbar sein.
        check_exports(&module)?;

        let mut store = Store::new(&self.engine, host_state(&limits, policy.clone()));
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
            notes: Vec::new(),
        })
    }

    /// Lädt ein Plugin aus einer Datei, mit der Gewährung aus der Policy-Datei
    /// (`[plugin.<name>]`, Sepp Guard): effektiv gilt der **Schnitt** aus Manifest-Anfrage und
    /// Gewährung. Ohne Gewährung (`None`) ist der Schnitt leer — das Plugin bekommt keine
    /// Rechte, und eines, das im Manifest etwas fordert, lädt nicht.
    pub fn load_file_with_grant(
        &self,
        wasm_path: &Path,
        manifest_path: Option<&Path>,
        grant: Option<&Policy>,
    ) -> Result<WasmPlugin> {
        let wasm = std::fs::read(wasm_path)
            .map_err(|e| SeppError::Tool(format!("wasm read {}: {e}", wasm_path.display())))?;
        let mut notes = Vec::new();
        let (requested, limits) = match manifest_path {
            Some(p) => {
                let manifest = Manifest::from_file(p)?;
                // Die unterstützte Protokollversion gehört dem Host, nicht der Policy — deshalb
                // steht die Prüfung hier und nicht in `Manifest::parse`.
                if manifest.abi > PLUGIN_ABI {
                    return Err(SeppError::Tool(format!(
                        "wasm: Plugin braucht Protokoll-Version {} (Feld `abi`), dieser sepp spricht {PLUGIN_ABI}",
                        manifest.abi
                    )));
                }
                let unknown = manifest.unknown_keys();
                if !unknown.is_empty() {
                    notes.push(format!(
                        "unbekannte Felder im Manifest, ohne Wirkung: {}",
                        unknown.join(", ")
                    ));
                }
                (manifest.policy(), manifest.limits.clone())
            }
            None => (Policy::default(), Limits::default()),
        };
        let policy = match grant {
            Some(g) => g.intersect(&requested),
            None => Policy::default(),
        };
        let mut plugin = self.load(&wasm, policy, limits)?;
        plugin.notes = notes;
        Ok(plugin)
    }

    /// Findet `*.wasm` in `dir` (Manifest: `<stem>.toml` oder `manifest.toml` daneben) und lädt
    /// sie mit der Gewährung aus `[plugin.<name>]`, aufgelöst über `grant_for(name)` (Name aus
    /// dem Manifest, sonst der Dateistamm). Ohne Gewährung bekommt das Plugin keine Rechte.
    ///
    /// Liefert die geladenen Plugins **und** je übersprungenem Plugin eine erklärende Zeile.
    /// Der zweite Rückgabewert existiert, weil das Frontend die Meldung sehen muss: Die TUI
    /// initialisiert kein Tracing, ein geloggter Fehler verschwände dort spurlos.
    pub fn discover_with(
        &self,
        dir: &Path,
        grant_for: &dyn Fn(&str) -> Option<Policy>,
    ) -> (Vec<WasmPlugin>, Vec<String>) {
        let mut out = Vec::new();
        let mut notes = Vec::new();
        let Ok(rd) = std::fs::read_dir(dir) else {
            return (out, notes);
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
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("plugin")
                .to_string();
            // Ein unlesbares Manifest fiele auf den Dateistamm zurück und ergäbe damit den
            // falschen Akteur — also nicht die Rechte, die gemeint waren. Das muss auffallen.
            let parsed = manifest.as_deref().map(Manifest::from_file);
            if let Some(Err(e)) = &parsed {
                notes.push(format!(
                    "WASM-Plugin {}: Manifest nicht lesbar ({e}) — der Name fällt auf \"{stem}\" \
                     zurück, Rechte werden unter diesem Namen gesucht",
                    path.display()
                ));
            }
            let name = parsed
                .and_then(|r| r.ok())
                .map(|m| m.name)
                .unwrap_or_else(|| stem.clone());
            let grant = grant_for(&name);
            match self.load_file_with_grant(&path, manifest.as_deref(), grant.as_ref()) {
                Ok(mut p) => {
                    for n in p.take_notes() {
                        notes.push(format!("WASM-Plugin {}: {n}", path.display()));
                    }
                    out.push(p);
                }
                Err(e) => {
                    tracing::warn!("wasm-plugin {} übersprungen: {e}", path.display());
                    // Der Hinweis auf die fehlende Gewährung passt nur, wenn das Modul an einem
                    // Import gescheitert ist. Bei einem ABI-Konflikt oder einem fehlenden Export
                    // führte er in die Irre.
                    let hint = if grant.is_none() && e.to_string().contains("instantiate") {
                        format!(
                            " — es gibt keinen Abschnitt [plugin.{name}] in der policy.toml, \
                             das Plugin bekommt deshalb keine Rechte"
                        )
                    } else {
                        String::new()
                    };
                    notes.push(format!(
                        "WASM-Plugin {} übersprungen: {e}{hint}",
                        path.display()
                    ));
                }
            }
        }
        (out, notes)
    }
}

/// Ein geladenes WASM-Plugin, exponiert als [`Tool`].
pub struct WasmPlugin {
    engine: Engine,
    module: Module,
    policy: Policy,
    limits: Limits,
    spec: ToolSpec,
    /// Hinweise aus dem Ladevorgang, die das Frontend zeigen soll (etwa unbekannte
    /// Manifest-Felder). Kein Fehler, aber auch nichts, das still bleiben darf.
    notes: Vec<String>,
}

impl WasmPlugin {
    /// Hinweise aus dem Ladevorgang (siehe [`WasmPlugin::notes`]).
    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    /// Effektive Policy des Plugins (Manifest-Anfrage, ggf. mit Gewährung geschnitten).
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

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
        let mut store = Store::new(engine, host_state(limits, policy.clone()));
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
  (import "env" "host_http" (func $http (param i32 i32) (result i32)))
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

    #[test]
    fn grant_intersection_removes_net_import() {
        // Das Manifest VERLANGT net, die Gewährung entscheidet. Ohne Gewährung gibt es nichts:
        // das Manifest ist die Selbstauskunft des Autors, keine Grenze.
        let tmp = tempfile::tempdir().unwrap();
        let wasm = tmp.path().join("netter.wasm");
        std::fs::write(&wasm, net_wat()).unwrap();
        let manifest = tmp.path().join("netter.toml");
        std::fs::write(
            &manifest,
            "name = \"netter\"\nkind = \"wasm\"\n[capabilities]\nnet = [\"example.com\"]\n",
        )
        .unwrap();
        let host = WasmHost::new();

        // Gewährung ohne net → Schnitt ohne net → host_http fehlt → lädt nicht.
        let denied = host.load_file_with_grant(&wasm, Some(&manifest), Some(&Policy::default()));
        assert!(
            denied.is_err(),
            "ohne Gewährung darf das Plugin nicht laden"
        );

        // Gewährung mit net → lädt; effektive Policy enthält genau den gewährten Host.
        let grant = Policy::new(vec![Capability::Net {
            host: "example.com".into(),
        }]);
        let ok = host
            .load_file_with_grant(&wasm, Some(&manifest), Some(&grant))
            .unwrap();
        assert!(ok.policy().net_allowed());

        // Keine Gewährung → keine Rechte → host_http fehlt → lädt ebenfalls nicht.
        assert!(
            host.load_file_with_grant(&wasm, Some(&manifest), None)
                .is_err(),
            "ohne Abschnitt in der policy.toml zählt das Manifest allein nicht"
        );

        // discover_with: leere Gewährung und gar keine Gewährung führen beide zum Überspringen,
        // aber nur die fehlende Gewährung nennt den fehlenden Abschnitt.
        let (plugins, notes) =
            host.discover_with(tmp.path(), &|name| (name == "netter").then(Policy::default));
        assert!(plugins.is_empty());
        assert_eq!(notes.len(), 1);
        assert!(!notes[0].contains("[plugin.netter]"), "{}", notes[0]);

        let (plugins, notes) = host.discover_with(tmp.path(), &|_| None);
        assert!(plugins.is_empty(), "ohne Gewährung lädt nichts");
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("[plugin.netter]"),
            "die Meldung nennt den fehlenden Abschnitt: {}",
            notes[0]
        );

        // Mit passender Gewährung lädt es über discover_with.
        let (plugins, notes) = host.discover_with(tmp.path(), &|name| {
            (name == "netter").then(|| {
                Policy::new(vec![Capability::Net {
                    host: "example.com".into(),
                }])
            })
        });
        assert_eq!(plugins.len(), 1);
        assert!(notes.is_empty());
    }

    #[test]
    fn unreadable_manifest_is_reported_not_swallowed() {
        // Ein kaputtes Manifest fällt beim Namen auf den Dateistamm zurück. Unter der Regel
        // „ohne Abschnitt keine Rechte" hieße das: Rechte werden unter dem falschen Namen
        // gesucht. Das darf nicht still passieren.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("kaputt.wasm"), compute_wat()).unwrap();
        std::fs::write(tmp.path().join("kaputt.toml"), "das ist kein toml [[[").unwrap();
        let host = WasmHost::new();
        let (_plugins, notes) = host.discover_with(tmp.path(), &|_| None);
        assert!(
            notes.iter().any(|n| n.contains("Manifest nicht lesbar")),
            "{notes:?}"
        );
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

    // ── ABI Version 1 ────────────────────────────────────────────────────────────────────

    #[test]
    fn missing_export_is_caught_at_load_time() {
        // Früher fiel ein Plugin ohne `sepp_alloc` erst beim ersten Werkzeug-Aufruf um. Für ein
        // Paket, das jemand installiert, muss „kaputt" beim Laden sichtbar sein.
        let spec = r#"{"name":"x","label":"X","description":"x","parameters":{"type":"object"}}"#;
        let wat = format!(
            r#"(module
  (memory (export "memory") 1)
  (data (i32.const 8) "{spec}")
  (func (export "sepp_spec") (result i64)
    (i64.or (i64.shl (i64.const 8) (i64.const 32)) (i64.const {speclen})))
  (func (export "sepp_call") (param i32) (param i32) (result i64) (i64.const 0))
)"#,
            spec = esc(spec),
            speclen = spec.len(),
        );
        let wasm = wat::parse_str(&wat).expect("wat");
        let msg = match WasmHost::new().load(&wasm, Policy::default(), Limits::default()) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("Modul ohne sepp_alloc darf nicht laden"),
        };
        assert!(msg.contains("sepp_alloc"), "{msg}");
        assert!(msg.contains("fehlt"), "{msg}");
    }

    #[test]
    fn wrong_export_signature_names_both_types() {
        let spec = r#"{"name":"x","label":"X","description":"x","parameters":{"type":"object"}}"#;
        // sepp_alloc mit falscher Signatur: (i32) -> i64 statt (i32) -> i32.
        let wat = format!(
            r#"(module
  (memory (export "memory") 1)
  (data (i32.const 8) "{spec}")
  (func (export "sepp_alloc") (param i32) (result i64) (i64.const 0))
  (func (export "sepp_spec") (result i64)
    (i64.or (i64.shl (i64.const 8) (i64.const 32)) (i64.const {speclen})))
  (func (export "sepp_call") (param i32) (param i32) (result i64) (i64.const 0))
)"#,
            spec = esc(spec),
            speclen = spec.len(),
        );
        let wasm = wat::parse_str(&wat).expect("wat");
        let msg = match WasmHost::new().load(&wasm, Policy::default(), Limits::default()) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("falsche Signatur darf nicht laden"),
        };
        assert!(msg.contains("falsche Signatur"), "{msg}");
        assert!(msg.contains("I64") && msg.contains("I32"), "{msg}");
    }

    #[test]
    fn newer_abi_is_rejected_with_the_supported_version() {
        let tmp = tempfile::tempdir().unwrap();
        let wasm = tmp.path().join("p.wasm");
        std::fs::write(&wasm, compute_wat()).unwrap();
        let manifest = tmp.path().join("p.toml");
        std::fs::write(
            &manifest,
            format!("name = \"p\"\nabi = {}\n", PLUGIN_ABI + 1),
        )
        .unwrap();

        let msg = match WasmHost::new().load_file_with_grant(&wasm, Some(&manifest), None) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("neueres ABI darf nicht laden"),
        };
        assert!(msg.contains(&(PLUGIN_ABI + 1).to_string()), "{msg}");
        assert!(msg.contains(&PLUGIN_ABI.to_string()), "{msg}");
    }

    #[test]
    fn unknown_manifest_field_loads_but_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("p.wasm"), compute_wat()).unwrap();
        // `capabilites` ist ein Tippfehler — er darf nicht still verschwinden.
        std::fs::write(
            tmp.path().join("p.toml"),
            "name = \"p\"\n[capabilites]\nnet = [\"x\"]\n",
        )
        .unwrap();

        let (plugins, notes) = WasmHost::new().discover_with(tmp.path(), &|_| None);
        assert_eq!(plugins.len(), 1, "das Plugin lädt trotzdem");
        assert!(notes.iter().any(|n| n.contains("capabilites")), "{notes:?}");
    }

    /// Ein Modul, das eine Datei liest: Anfrage bauen, `host_fs_read` rufen, mit
    /// `host_result_read` abholen und das Ergebnis als Werkzeug-Ergebnis zurückgeben.
    /// Führt den kompletten Abholweg vor.
    fn fs_reader_wat(request: &str) -> Vec<u8> {
        let spec = r#"{"name":"reader","label":"Reader","description":"x","parameters":{"type":"object"}}"#;
        // Das Ergebnis der Fähigkeit ist ein Objekt und wandert deshalb nach `details`;
        // in `text` gehörte eine Zeichenkette.
        let prefix = r#"{"content":[{"type":"text","text":"ok"}],"details":"#;
        let suffix = r#"}"#;
        let wat = format!(
            r#"(module
  (import "env" "host_fs_read" (func $fs (param i32 i32) (result i32)))
  (import "env" "host_result_read" (func $read (param i32 i32) (result i32)))
  (memory (export "memory") 4)
  (data (i32.const 8) "{spec}")
  (data (i32.const 2048) "{req}")
  (data (i32.const 3072) "{prefix}")
  (data (i32.const 3584) "{suffix}")
  (global $bump (mut i32) (i32.const 4096))
  (func (export "sepp_alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $n)))
    (local.get $p))
  (func (export "sepp_spec") (result i64)
    (i64.or (i64.shl (i64.const 8) (i64.const 32)) (i64.const {speclen})))
  (func (export "sepp_call") (param i32) (param i32) (result i64)
    (local $n i32) (local $got i32) (local $out i32) (local $cur i32)
    ;; Fähigkeit rufen: liefert die Größe des Ergebnisses.
    (local.set $n (call $fs (i32.const 2048) (i32.const {reqlen})))
    ;; Ergebnis in einen selbst gestellten Puffer holen.
    (local.set $got (call $read (i32.const 65536) (local.get $n)))
    ;; Als Text-Ergebnis verpacken: prefix + geholtes JSON + suffix.
    (local.set $out (i32.const 131072))
    (memory.copy (local.get $out) (i32.const 3072) (i32.const {plen}))
    (local.set $cur (i32.add (local.get $out) (i32.const {plen})))
    (memory.copy (local.get $cur) (i32.const 65536) (local.get $got))
    (local.set $cur (i32.add (local.get $cur) (local.get $got)))
    (memory.copy (local.get $cur) (i32.const 3584) (i32.const {slen}))
    (i64.or (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
            (i64.extend_i32_u (i32.add (i32.add (i32.const {plen}) (local.get $got))
                                       (i32.const {slen})))))
)"#,
            spec = esc(spec),
            speclen = spec.len(),
            req = esc(request),
            reqlen = request.len(),
            prefix = esc(prefix),
            plen = prefix.len(),
            suffix = esc(suffix),
            slen = suffix.len(),
        );
        wat::parse_str(&wat).expect("fs reader wat")
    }

    #[tokio::test]
    async fn host_fs_read_reads_allowed_file_through_the_full_pickup_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("daten.txt");
        std::fs::write(&file, "Hallo Welt").unwrap();
        let req = format!(r#"{{"path":"{}"}}"#, file.display());

        let grant = Policy::new(vec![Capability::FsRead {
            prefix: dir.path().canonicalize().unwrap(),
        }]);
        let plugin = WasmHost::new()
            .load(&fs_reader_wat(&req), grant, Limits::default())
            .expect("lädt mit FsRead-Gewährung");

        let res = plugin
            .execute(serde_json::json!({}), CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(res.details["text"], "Hallo Welt");
        assert_eq!(res.details["bytes"], 10);
        assert_eq!(res.details["lossy"], false);
    }

    #[tokio::test]
    async fn host_fs_read_refuses_path_outside_the_grant() {
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("geheim.txt");
        std::fs::write(&secret, "streng geheim").unwrap();
        let req = format!(r#"{{"path":"{}"}}"#, secret.display());

        let grant = Policy::new(vec![Capability::FsRead {
            prefix: inside.path().canonicalize().unwrap(),
        }]);
        let plugin = WasmHost::new()
            .load(&fs_reader_wat(&req), grant, Limits::default())
            .expect("lädt, das Gate hängt am Import");

        let res = plugin
            .execute(serde_json::json!({}), CancellationToken::new(), None)
            .await
            .unwrap();
        let err = res.details["error"].as_str().unwrap_or_default();
        assert!(err.contains("außerhalb der Rechte"), "{err}");
        assert!(
            !res.details.to_string().contains("streng geheim"),
            "der Inhalt darf nicht durchsickern: {}",
            res.details
        );
    }

    #[tokio::test]
    async fn host_fs_read_reports_missing_file_as_error_not_trap() {
        let dir = tempfile::tempdir().unwrap();
        let req = format!(r#"{{"path":"{}/gibtsnicht.txt"}}"#, dir.path().display());
        let grant = Policy::new(vec![Capability::FsRead {
            prefix: dir.path().canonicalize().unwrap(),
        }]);
        let plugin = WasmHost::new()
            .load(&fs_reader_wat(&req), grant, Limits::default())
            .unwrap();
        let res = plugin
            .execute(serde_json::json!({}), CancellationToken::new(), None)
            .await
            .expect("ein fehlender Pfad darf das Plugin nicht abstürzen lassen");
        assert!(res.details["error"].is_string(), "{}", res.details);
    }

    /// Baut das Beispiel-Plugin aus `examples/textstat-plugin` und führt es aus.
    ///
    /// Prüft den ganzen Weg vom Rust-Quelltext bis zum Werkzeug-Ergebnis und hält damit fest,
    /// dass das Beispiel zum ABI passt. `#[ignore]`, weil die CI kein WASM-Target installiert
    /// hat und ein Toolchain-Zwang für alle Mitwirkenden unverhältnismäßig wäre:
    ///
    /// ```bash
    /// cargo test -p sepp-wasm -- --ignored
    /// ```
    #[tokio::test]
    #[ignore = "braucht das Target wasm32-unknown-unknown"]
    async fn example_plugin_builds_and_runs() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/textstat-plugin");
        let out = std::process::Command::new("cargo")
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-unknown-unknown",
                "--manifest-path",
            ])
            .arg(dir.join("Cargo.toml"))
            .output()
            .expect("cargo startbar");
        assert!(
            out.status.success(),
            "Beispiel-Plugin baut nicht:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let wasm = dir.join("target/wasm32-unknown-unknown/release/textstat.wasm");
        let manifest = dir.join("textstat.toml");
        // Ohne Gewährung — das Beispiel fordert nichts an und muss trotzdem laden.
        let plugin = WasmHost::new()
            .load_file_with_grant(&wasm, Some(&manifest), None)
            .expect("Beispiel-Plugin lädt");
        assert_eq!(plugin.spec().name, "textstat");

        let res = plugin
            .execute(
                serde_json::json!({ "text": "Hallo Welt\nZweite Zeile" }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("Aufruf gelingt");
        assert!(!res.is_error, "{res:?}");
        let sepp_core::ContentBlock::Text { text } = &res.content[0] else {
            panic!("Textblock erwartet: {res:?}")
        };
        assert!(text.contains("2 Zeilen"), "{text}");
        assert!(text.contains("4 Wörter"), "{text}");
        assert_eq!(res.details["words"], 4);

        // Ungültige Eingabe wird zum Fehler-Ergebnis, nicht zum Absturz.
        let bad = plugin
            .execute(serde_json::json!({}), CancellationToken::new(), None)
            .await
            .expect("auch der Fehlerfall liefert ein Ergebnis");
        assert!(bad.is_error, "{bad:?}");
    }
}
