//! Live-Probe für einen MCP-Server (streamable HTTP).
//!
//! Verbindet, listet Tools und ruft optional eines auf — ohne LLM/API-Key.
//!
//! ```bash
//! cargo run -p sepp-mcp --example probe -- https://api.fpv7.com/mcp
//! cargo run -p sepp-mcp --example probe -- https://api.fpv7.com/mcp search '{"q":"motors"}'
//! ```

use std::collections::HashSet;

use sepp_mcp::{connect, McpServerConfig};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .expect("Verwendung: probe <url> [tool] [json-args]");
    let tool_name = args.next();
    let tool_args = args.next();

    let cfg = McpServerConfig {
        name: "probe".into(),
        transport: "http".into(),
        url: Some(url.clone()),
        command: vec![],
        capabilities: Default::default(),
    };

    println!("Verbinde mit {url} …");
    let conn = connect(&cfg).await?;
    println!(
        "Verbunden mit '{}': {} Tools\n",
        conn.server(),
        conn.tool_count()
    );

    let mut taken = HashSet::new();
    let tools = conn.into_tools(&mut taken);
    for t in &tools {
        let s = t.spec();
        let desc: String = s.description.chars().take(90).collect();
        println!("  {} — {desc}", s.name);
    }

    if let Some(name) = tool_name {
        let args_val: serde_json::Value = match tool_args {
            Some(s) => serde_json::from_str(&s)?,
            None => serde_json::json!({}),
        };
        let Some(tool) = tools.iter().find(|t| t.spec().name == name) else {
            anyhow::bail!("Tool '{name}' nicht gefunden");
        };
        println!("\nRufe '{name}' auf mit {args_val} …");
        let r = tool
            .execute(args_val, CancellationToken::new(), None)
            .await?;
        println!("\nErgebnis ({}):", if r.is_error { "FEHLER" } else { "ok" });
        for b in &r.content {
            if let sepp_core::ContentBlock::Text { text } = b {
                let shown: String = text.chars().take(2000).collect();
                println!("{shown}");
            }
        }
    }
    Ok(())
}
