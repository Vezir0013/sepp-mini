//! `textstat` — ein Beispiel-Plugin für sepp mini (Tier 2, WASM).
//!
//! Zählt Zeichen, Wörter und Zeilen eines Textes und schätzt die Tokenzahl. Es braucht weder
//! Netz noch Dateizugriff und deklariert deshalb keine Capabilities: Es läuft ohne einen
//! `[plugin.textstat]`-Abschnitt in der `policy.toml`.
//!
//! Zweck ist weniger die Statistik als das **Aufrufprotokoll**. Der Teil unter „Das Protokoll"
//! ist bei jedem Plugin gleich und darf kopiert werden; darunter kommt die eigentliche Arbeit.
//!
//! Bauen und installieren: siehe `README.md` daneben, oder `just plugin-example` im Repo-Root.

use serde::Deserialize;
use serde_json::json;

// ── Das Protokoll ─────────────────────────────────────────────────────────────────────────
//
// Der Host erwartet vier Exports: `memory`, `sepp_spec`, `sepp_alloc` und `sepp_call`.
// `memory` und `sepp_spec` prüft er beim Laden, die beiden anderen erst beim ersten
// Werkzeug-Aufruf. Ein Plugin ohne `sepp_alloc` lädt also scheinbar sauber und fällt erst
// später um — beim Suchen nach dem Fehler ist das die erste Stelle zum Nachsehen.

/// Packt Zeiger und Länge in den Rückgabewert: oberes Wort Zeiger, unteres Wort Länge.
///
/// Die Zwischenstufe `as u32` ist nicht kosmetisch. Ohne sie würde ein `i32` mit gesetztem
/// höchsten Bit beim Verbreitern sein Vorzeichen in die oberen 32 Bit schmieren und die Länge
/// zerstören.
fn pack(ptr: usize, len: usize) -> i64 {
    ((ptr as u32 as i64) << 32) | (len as u32 as i64)
}

/// Reserviert `n` Bytes und liefert die Adresse im linearen Speicher.
///
/// Der Host ruft das selbst auf, um die Argumente hineinzuschreiben, bevor er `sepp_call`
/// aufruft. Das `forget` ist Absicht und der Kern des Protokolls: Es gibt **keinen**
/// Freigabe-Aufruf, der Puffer gehört ab hier dem Host. Was hier belegt wird, bleibt belegt,
/// bis der Host die ganze Instanz verwirft — und das tut er nach jedem Werkzeug-Aufruf.
/// Ein Zustand über zwei Aufrufe hinweg ist deshalb ohnehin unmöglich.
#[no_mangle]
pub extern "C" fn sepp_alloc(n: i32) -> i32 {
    let mut buf: Vec<u8> = Vec::with_capacity(n.max(0) as usize);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr as i32
}

/// Legt `bytes` im linearen Speicher ab und packt Adresse und Länge für die Rückgabe.
///
/// Der Puffer muss die Rückkehr aus `sepp_call` überleben, denn der Host liest ihn erst
/// danach. Also auch hier: bewusst nicht freigeben.
fn emit(bytes: &[u8]) -> i64 {
    let mut buf = bytes.to_vec();
    let ptr = buf.as_mut_ptr() as usize;
    let len = buf.len();
    std::mem::forget(buf);
    pack(ptr, len)
}

/// Die Werkzeugbeschreibung, die das Modell zu sehen bekommt.
///
/// Alle vier Felder sind Pflicht. `parameters` ist ein JSON-Schema und wird unverändert an den
/// Anbieter durchgereicht, deshalb schlank halten: kein `$schema`, kein `title`.
const SPEC: &str = r#"{
  "name": "textstat",
  "label": "Textstatistik",
  "description": "Zählt Zeichen, Wörter und Zeilen eines Textes und schätzt die Tokenzahl.",
  "parameters": {
    "type": "object",
    "properties": {
      "text": { "type": "string", "description": "Der zu vermessende Text." }
    },
    "required": ["text"]
  }
}"#;

/// Liefert die Werkzeugbeschreibung. Wird einmal beim Laden aufgerufen.
#[no_mangle]
pub extern "C" fn sepp_spec() -> i64 {
    emit(SPEC.as_bytes())
}

// `host_log` schreibt eine Zeile ins Log des Hosts. Immer verfügbar, ohne jede Gewährung.
//
// Die beiden anderen Host-Funktionen, `host_fs_read` und `host_http`, dürfen hier NICHT
// deklariert werden: Der Host registriert sie nur bei passender Gewährung, und ein Import ohne
// sie verhindert die Instanziierung. Das Plugin würde dann gar nicht laden.
extern "C" {
    fn host_log(ptr: i32, len: i32);
}

fn log(msg: &str) {
    unsafe { host_log(msg.as_ptr() as i32, msg.len() as i32) }
}

/// Führt das Werkzeug aus: Argumente als JSON hinein, Ergebnis als JSON hinaus.
///
/// # Safety
/// `ptr` und `len` beschreiben den Puffer, den der Host zuvor über [`sepp_alloc`] belegt und
/// mit den Argumenten gefüllt hat.
#[no_mangle]
pub unsafe extern "C" fn sepp_call(ptr: i32, len: i32) -> i64 {
    let raw = std::slice::from_raw_parts(ptr as *const u8, len.max(0) as usize);
    emit(run(raw).as_bytes())
}

// ── Die eigentliche Arbeit ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Args {
    text: String,
}

/// Ein Fehler-Ergebnis. Ein Plugin sollte nie in einen Trap laufen: Das Modell kann mit einer
/// Fehlermeldung etwas anfangen, mit einem abgestürzten Werkzeug nicht.
fn error(msg: &str) -> String {
    json!({
        "content": [{ "type": "text", "text": msg }],
        "is_error": true
    })
    .to_string()
}

fn run(raw: &[u8]) -> String {
    let args: Args = match serde_json::from_slice(raw) {
        Ok(a) => a,
        Err(e) => return error(&format!("textstat: ungültige Parameter: {e}")),
    };
    log(&format!("textstat: {} Bytes erhalten", args.text.len()));

    let chars = args.text.chars().count();
    let words = args.text.split_whitespace().count();
    let lines = if args.text.is_empty() {
        0
    } else {
        args.text.lines().count()
    };
    // Dieselbe grobe Heuristik, mit der sepp sein Kontext-Budget rechnet: vier Bytes je Token.
    let tokens = args.text.len() / 4;

    let text = format!(
        "{lines} Zeilen · {words} Wörter · {chars} Zeichen · ~{tokens} Tokens geschätzt"
    );
    // `details` geht an die Oberfläche, nicht ans Modell — gut für Zahlen, die man weiter-
    // verarbeiten will, ohne das Kontextfenster mit JSON zu füllen.
    json!({
        "content": [{ "type": "text", "text": text }],
        "details": {
            "lines": lines, "words": words, "chars": chars, "tokens_estimated": tokens
        }
    })
    .to_string()
}
