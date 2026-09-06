//! `textstat` — ein Beispiel-Plugin für sepp mini (Tier 2, WASM), geschrieben mit dem SDK
//! `sepp-plugin`.
//!
//! Zählt Zeichen, Wörter und Zeilen eines Textes und schätzt die Tokenzahl. Es braucht weder
//! Netz noch Dateizugriff und deklariert deshalb keine Capabilities: Es läuft ohne einen
//! `[plugin.textstat]`-Abschnitt in der `policy.toml`.
//!
//! Das Aufrufprotokoll des Hosts (Exports, Zeiger, Abholweg) übernimmt das Attribut
//! `#[sepp_plugin::tool]`; hier steht nur noch die Arbeit. Bauen und installieren: siehe
//! `README.md` daneben, oder `just plugin-example` im Repo-Root.

use sepp_plugin::prelude::*;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Der zu vermessende Text.
    text: String,
}

#[sepp_plugin::tool(
    desc = "Zählt Zeichen, Wörter und Zeilen eines Textes und schätzt die Tokenzahl.",
    label = "Textstatistik"
)]
fn textstat(args: Args, host: &Host) -> Result<ToolResult> {
    host.log(&format!("textstat: {} Bytes erhalten", args.text.len()));

    let chars = args.text.chars().count();
    let words = args.text.split_whitespace().count();
    let lines = if args.text.is_empty() {
        0
    } else {
        args.text.lines().count()
    };
    // Dieselbe grobe Heuristik, mit der sepp sein Kontext-Budget rechnet: vier Bytes je Token.
    let tokens = args.text.len() / 4;

    let text =
        format!("{lines} Zeilen · {words} Wörter · {chars} Zeichen · ~{tokens} Tokens geschätzt");
    // `details` geht an die Oberfläche, nicht ans Modell — gut für Zahlen, die man weiter-
    // verarbeiten will, ohne das Kontextfenster mit JSON zu füllen.
    Ok(ToolResult::text(text).with_details(json!({
        "lines": lines, "words": words, "chars": chars, "tokens_estimated": tokens
    })))
}
