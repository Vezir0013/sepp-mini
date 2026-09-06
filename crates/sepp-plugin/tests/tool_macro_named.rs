//! `name`/`label` überschreiben die Defaults; ein `Err` der Autorfunktion wird zum
//! Fehler-Ergebnis mit dem Werkzeugnamen vorn.

use sepp_plugin::prelude::*;

#[derive(Deserialize, JsonSchema)]
struct Args {
    #[allow(dead_code)]
    text: String,
}

#[sepp_plugin::tool(desc = "Scheitert immer.", name = "anders", label = "Anders")]
fn kaputt(_args: Args, _host: &Host) -> Result<ToolResult> {
    Err("kaputt".into())
}

#[test]
fn name_and_label_are_taken_from_the_attribute() {
    let spec: ToolSpec = serde_json::from_str(&__sepp_plugin_export::spec_json()).unwrap();
    assert_eq!(spec.name, "anders");
    assert_eq!(spec.label, "Anders");
}

#[test]
fn an_err_is_reported_under_the_tool_name() {
    let out = __sepp_plugin_export::call_json(br#"{"text":"x"}"#);
    let r: ToolResult = serde_json::from_str(&out).unwrap();
    assert!(r.is_error);
    let ContentBlock::Text { text } = &r.content[0] else {
        panic!()
    };
    assert_eq!(text, "anders: kaputt");
}
