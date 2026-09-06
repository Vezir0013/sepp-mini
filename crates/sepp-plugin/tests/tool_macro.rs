//! Das Makro nativ: Ein Werkzeug wie im Zielbild, geprüft über das erzeugte Modul
//! `__sepp_plugin_export` — so testet auch ein Autor sein Plugin ohne wasm32-Target.

use sepp_plugin::prelude::*;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Der zu zählende Text.
    text: String,
}

#[sepp_plugin::tool(desc = "Zählt die Wörter eines Textes.")]
fn woerter(args: Args, host: &Host) -> Result<ToolResult> {
    host.log("zähle");
    let n = args.text.split_whitespace().count();
    Ok(ToolResult::text(format!("{n} Wörter")).with_details(json!({ "n": n })))
}

#[test]
fn spec_is_derived_from_the_function_and_its_args() {
    let spec: ToolSpec = serde_json::from_str(&__sepp_plugin_export::spec_json()).unwrap();
    assert_eq!(spec.name, "woerter");
    assert_eq!(spec.label, "woerter");
    assert_eq!(spec.description, "Zählt die Wörter eines Textes.");
    assert_eq!(spec.parameters["required"], json!(["text"]));
    assert_eq!(
        spec.parameters["properties"]["text"]["description"],
        "Der zu zählende Text."
    );
    assert!(spec.parameters.get("$schema").is_none());
    assert!(spec.parameters.get("title").is_none());
}

#[test]
fn call_round_trips_through_json() {
    let out = __sepp_plugin_export::call_json(br#"{"text":"Hallo Welt hier"}"#);
    let r: ToolResult = serde_json::from_str(&out).unwrap();
    assert!(!r.is_error, "{r:?}");
    assert_eq!(r.details["n"], 3);
    let ContentBlock::Text { text } = &r.content[0] else {
        panic!("Textblock erwartet: {r:?}")
    };
    assert_eq!(text, "3 Wörter");
}

#[test]
fn bad_arguments_become_an_error_result_not_a_trap() {
    let r: ToolResult = serde_json::from_str(&__sepp_plugin_export::call_json(b"{}")).unwrap();
    assert!(r.is_error, "{r:?}");
    let ContentBlock::Text { text } = &r.content[0] else {
        panic!()
    };
    assert!(text.starts_with("woerter: ungültige Parameter:"), "{text}");
}
