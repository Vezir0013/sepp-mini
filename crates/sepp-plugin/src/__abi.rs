//! Die Kodierung des Plugin-ABI 1 — das, was ein Autor früher von Hand schrieb.
//!
//! Öffentlich nur, weil das Makro `#[tool]` im Crate des Autors darauf zeigt (`#[doc(hidden)]`).
//! Die reinen Teile (`pack`, `spec_json`, `call_json`, `decode_*`) laufen auch nativ und sind
//! dort getestet; alles, was Zeiger in den linearen Speicher anfasst (`alloc`, `emit`, `input`),
//! gibt es nur unter `wasm32` — nativ wäre `ptr as i32` auf 64 Bit verlustbehaftet.
//!
//! Vertragstext: `wit/sepp.wit` im Repo-Root, Abschnitt „Kodierung ABI 1".

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use sepp_core::{ContentBlock, ToolResult, ToolSpec};

use crate::error::{Error, Result};
use crate::host::Host;

/// Packt Zeiger und Länge in den Rückgabewert: oberes Wort Zeiger, unteres Wort Länge.
///
/// Die Zwischenstufe `as u32` ist nicht kosmetisch: Ohne sie schmierte ein `i32` mit gesetztem
/// höchsten Bit beim Verbreitern sein Vorzeichen in die oberen 32 Bit und zerstörte die Länge.
pub fn pack(ptr: usize, len: usize) -> i64 {
    ((ptr as u32 as i64) << 32) | (len as u32 as i64)
}

/// Ein Fehler-Ergebnis: Text ans Modell, `is_error = true`. Ein Plugin trappt nie — das Modell
/// kann mit einer Erklärung etwas anfangen, mit einem abgestürzten Werkzeug nicht.
pub fn error_result(message: impl Into<String>) -> ToolResult {
    ToolResult {
        content: vec![ContentBlock::text(message)],
        details: Value::Null,
        is_error: true,
    }
}

/// Die Werkzeugbeschreibung als JSON — der Inhalt von `sepp_spec`. Das Parameter-Schema kommt
/// aus `A` über `sepp_core::schema_for` (ohne `$schema`/`title`, wie bei den eingebauten Tools).
pub fn spec_json<A: JsonSchema>(name: &str, label: &str, desc: &str) -> String {
    let spec = ToolSpec {
        name: name.to_owned(),
        label: label.to_owned(),
        description: desc.to_owned(),
        parameters: sepp_core::schema_for::<A>(),
    };
    serde_json::to_string(&spec).unwrap_or_else(|_| {
        // Strings und ein `Value` lassen sich immer serialisieren; der Zweig ist Vorsicht, kein
        // erwarteter Pfad — und `Value::to_string` kann nicht scheitern.
        serde_json::json!({
            "name": name, "label": label, "description": desc,
            "parameters": { "type": "object" }
        })
        .to_string()
    })
}

/// Ein Aufruf: Argument-JSON hinein, ToolResult-JSON hinaus — der Inhalt von `sepp_call`.
///
/// Ungültige Argumente und ein `Err` der Autorfunktion werden zu einem Ergebnis mit
/// `is_error = true`, jeweils mit dem Werkzeugnamen vorn, damit das Modell weiß, wer spricht.
pub fn call_json<A, F>(name: &str, raw: &[u8], f: F) -> String
where
    A: DeserializeOwned,
    F: FnOnce(A, &Host) -> Result<ToolResult>,
{
    let result = match serde_json::from_slice::<A>(raw) {
        Ok(args) => match f(args, &Host::default()) {
            Ok(r) => r,
            Err(e) => error_result(format!("{name}: {e}")),
        },
        Err(e) => error_result(format!("{name}: ungültige Parameter: {e}")),
    };
    serde_json::to_string(&result).unwrap_or_else(|e| {
        serde_json::json!({
            "content": [{ "type": "text", "text": format!("{name}: Ergebnis nicht serialisierbar: {e}") }],
            "is_error": true
        })
        .to_string()
    })
}

/// Deutet den Rückgabewert einer **rohen** Fähigkeit (`host_fs_read_bytes`): `n >= 0` sind `n`
/// Bytes Nutzdaten, `n < 0` sind `-n - 1` Bytes UTF-8-Fehlertext — die Kodierung von
/// `result<list<u8>, string>`. `fetch(len)` holt die Bytes über `host_result_read`.
pub fn decode_raw(n: i32, fetch: impl FnOnce(usize) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    if n >= 0 {
        return fetch(n as usize);
    }
    // In i64 rechnen: `-i32::MIN - 1` passt nicht in i32.
    let len = (-(n as i64) - 1) as usize;
    let text = fetch(len)?;
    if text.is_empty() {
        return Err(Error::new("die Fähigkeit meldete einen Fehler ohne Text"));
    }
    Err(Error::new(String::from_utf8_lossy(&text).into_owned()))
}

/// Deutet den Rückgabewert einer **JSON**-Fähigkeit (`host_http`): `n` ist die Länge des
/// abgelegten JSON; ein Objekt mit `"error"` ist ein Fehler, alles andere wird zu `T`.
pub fn decode_json<T: DeserializeOwned>(
    n: i32,
    fetch: impl FnOnce(usize) -> Result<Vec<u8>>,
) -> Result<T> {
    if n < 0 {
        return Err(Error::new(format!("die Fähigkeit meldete {n}")));
    }
    let bytes = fetch(n as usize)?;
    let value: Value = serde_json::from_slice(&bytes)?;
    if let Some(err) = value.get("error").and_then(Value::as_str) {
        return Err(Error::new(err));
    }
    Ok(serde_json::from_value(value)?)
}

/// Reserviert `n` Bytes im linearen Speicher — der Host ruft das auf, um die Argumente
/// hineinzuschreiben. Das `forget` ist der Kern des Protokolls: Es gibt kein Freigeben, der
/// Puffer gehört ab hier dem Host, und die Instanz wird nach dem Aufruf verworfen.
#[cfg(target_arch = "wasm32")]
pub fn alloc(n: i32) -> i32 {
    let mut buf: Vec<u8> = Vec::with_capacity(n.max(0) as usize);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr as i32
}

/// Legt `s` im linearen Speicher ab und packt Adresse und Länge für die Rückgabe. Der Puffer
/// muss die Rückkehr aus `sepp_call` überleben, weil der Host ihn erst danach liest.
#[cfg(target_arch = "wasm32")]
pub fn emit(s: String) -> i64 {
    let mut bytes = s.into_bytes();
    bytes.shrink_to_fit();
    let ptr = bytes.as_mut_ptr() as usize;
    let len = bytes.len();
    std::mem::forget(bytes);
    pack(ptr, len)
}

/// Die Argumente, die der Host über `alloc` abgelegt hat, als Slice.
///
/// # Safety
/// `ptr` und `len` müssen den Puffer beschreiben, den der Host zuvor über `alloc` belegt und
/// gefüllt hat. Das ist die Zusage des ABI; `len <= 0` ergibt ein leeres Slice.
#[cfg(target_arch = "wasm32")]
pub unsafe fn input<'a>(ptr: i32, len: i32) -> &'a [u8] {
    if len <= 0 || ptr < 0 {
        return &[];
    }
    std::slice::from_raw_parts(ptr as u32 as usize as *const u8, len as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Args {
        /// Der Text.
        text: String,
        #[serde(default)]
        limit: Option<u32>,
    }

    fn zaehlen(args: Args, host: &Host) -> Result<ToolResult> {
        host.log("test");
        if args.text == "kaputt" {
            return Err("absichtlich".into());
        }
        let n = args.text.split_whitespace().count();
        Ok(ToolResult::text(format!("{n} Wörter")).with_details(serde_json::json!({ "n": n })))
    }

    #[test]
    fn pack_keeps_pointer_and_length_apart() {
        assert_eq!(pack(1, 2), (1i64 << 32) | 2);
        // Ein Zeiger mit gesetztem höchsten Bit darf die Länge nicht zerstören.
        let v = pack(0x8000_0000, 5);
        assert_eq!(v as u64 >> 32, 0x8000_0000);
        assert_eq!(v & 0xffff_ffff, 5);
    }

    #[test]
    fn spec_json_is_a_complete_tool_spec_without_schema_noise() {
        let spec: ToolSpec =
            serde_json::from_str(&spec_json::<Args>("zaehlen", "Zähler", "Zählt")).unwrap();
        assert_eq!(spec.name, "zaehlen");
        assert_eq!(spec.label, "Zähler");
        assert_eq!(spec.description, "Zählt");
        assert_eq!(spec.parameters["type"], "object");
        assert_eq!(spec.parameters["required"], serde_json::json!(["text"]));
        assert_eq!(
            spec.parameters["properties"]["text"]["description"],
            "Der Text."
        );
        assert!(spec.parameters.get("$schema").is_none());
        assert!(spec.parameters.get("title").is_none());
    }

    #[test]
    fn call_json_runs_the_tool_and_returns_its_result() {
        let out = call_json::<Args, _>("zaehlen", br#"{"text":"a b c"}"#, zaehlen);
        let r: ToolResult = serde_json::from_str(&out).unwrap();
        assert!(!r.is_error);
        assert_eq!(r.details["n"], 3);
        let ContentBlock::Text { text } = &r.content[0] else {
            panic!("Textblock erwartet: {r:?}")
        };
        assert_eq!(text, "3 Wörter");
    }

    #[test]
    fn call_json_turns_bad_arguments_and_errors_into_error_results() {
        let out = call_json::<Args, _>("zaehlen", b"{}", zaehlen);
        let r: ToolResult = serde_json::from_str(&out).unwrap();
        assert!(r.is_error);
        let ContentBlock::Text { text } = &r.content[0] else {
            panic!()
        };
        assert!(text.starts_with("zaehlen: ungültige Parameter:"), "{text}");
        assert!(text.contains("text"), "{text}");

        let out = call_json::<Args, _>("zaehlen", br#"{"text":"kaputt"}"#, zaehlen);
        let r: ToolResult = serde_json::from_str(&out).unwrap();
        assert!(r.is_error);
        let ContentBlock::Text { text } = &r.content[0] else {
            panic!()
        };
        assert_eq!(text, "zaehlen: absichtlich");

        // Kein JSON ist auch nur ein Fehler-Ergebnis, kein Trap.
        let out = call_json::<Args, _>("zaehlen", b"nicht json", zaehlen);
        let r: ToolResult = serde_json::from_str(&out).unwrap();
        assert!(r.is_error);
    }

    #[test]
    fn decode_raw_follows_the_sign_convention() {
        let bytes = decode_raw(3, |n| Ok(vec![1u8; n])).unwrap();
        assert_eq!(bytes, vec![1, 1, 1]);
        assert_eq!(
            decode_raw(0, |n| Ok(vec![0u8; n])).unwrap(),
            Vec::<u8>::new()
        );

        // -6 → fünf Bytes Fehlertext.
        let e = decode_raw(-6, |n| {
            assert_eq!(n, 5);
            Ok(b"Fehla".to_vec())
        })
        .unwrap_err();
        assert_eq!(e.message(), "Fehla");

        // -1 → null Bytes: Fehler ohne Text bekommt trotzdem eine Erklärung.
        let e = decode_raw(-1, |n| {
            assert_eq!(n, 0);
            Ok(Vec::new())
        })
        .unwrap_err();
        assert!(e.message().contains("ohne Text"), "{e}");

        // i32::MIN darf nicht überlaufen.
        let e = decode_raw(i32::MIN, |n| {
            assert_eq!(n, i32::MAX as usize);
            Ok(b"x".to_vec())
        })
        .unwrap_err();
        assert_eq!(e.message(), "x");

        // Ein Fehler beim Abholen reicht durch.
        assert!(decode_raw(3, |_| Err(Error::new("weg"))).is_err());
    }

    #[test]
    fn decode_json_distinguishes_error_objects_from_payloads() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Antwort {
            status: u16,
        }
        let ok: Antwort = decode_json(0, |_| Ok(br#"{"status":200}"#.to_vec())).unwrap();
        assert_eq!(ok, Antwort { status: 200 });

        let e = decode_json::<Antwort>(0, |_| Ok(br#"{"error":"nicht implementiert"}"#.to_vec()))
            .unwrap_err();
        assert_eq!(e.message(), "nicht implementiert");

        assert!(decode_json::<Antwort>(-1, |_| Ok(Vec::new())).is_err());
        assert!(decode_json::<Antwort>(0, |_| Ok(b"kein json".to_vec())).is_err());
    }
}
