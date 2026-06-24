//! `sepp-mcp` — Tier-3-Erweiterung: bindet **MCP-Server** als Tool-Quelle ein (via `rmcp`).
//!
//! Transporte: streamable HTTP und stdio (Child-Prozess). Jedes Remote-Tool wird als
//! [`McpTool`] (`impl sepp_tools::Tool`) exponiert; bei Namenskollision mit bereits
//! vergebenen Tools wird `<server>__<tool>` als Präfix genutzt. Konfiguration über
//! `[[mcp.servers]]` in `settings.toml`.
//!
//! Hinweis: Die OS-Sandbox für MCP-Subprozesse (`sepp-policy`) kommt in Phase 4; die
//! `allow_*`-Felder werden hier bereits geparst, aber noch nicht durchgesetzt.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::ServiceExt;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use sepp_core::{ContentBlock, ImageSource, Result, SeppError, ToolResult, ToolSpec};
use sepp_tools::Tool;

type Service = RunningService<RoleClient, ()>;

/// Obergrenze für einen einzelnen Tool-Aufruf (verhindert Hängen bei stummem Server).
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// `settings.toml`-Wurzel (nur der `mcp`-Teil interessiert hier).
#[derive(Debug, Default, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub mcp: McpSection,
}

#[derive(Debug, Default, Deserialize)]
pub struct McpSection {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// Ein konfigurierter MCP-Server (`[[mcp.servers]]`).
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    /// `"http"` oder `"stdio"`.
    pub transport: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub command: Vec<String>,
    /// `[mcp.servers.capabilities]` — werden für stdio-Subprozesse per Sandbox erzwungen.
    #[serde(default)]
    pub capabilities: sepp_policy::Capabilities,
}

/// Liest `[[mcp.servers]]` aus mehreren `settings.toml` (fehlende Dateien werden ignoriert).
pub fn load_settings(paths: &[std::path::PathBuf]) -> Result<Vec<McpServerConfig>> {
    let mut servers = Vec::new();
    for p in paths {
        let Ok(text) = std::fs::read_to_string(p) else {
            continue;
        };
        let settings: Settings = toml::from_str(&text)
            .map_err(|e| SeppError::Config(format!("settings {}: {e}", p.display())))?;
        servers.extend(settings.mcp.servers);
    }
    // Doppelte Server-Namen sind ein Konfigurationsfehler (mehrdeutige Präfixe).
    let mut seen = HashSet::new();
    for s in &servers {
        if !seen.insert(s.name.as_str()) {
            return Err(SeppError::Config(format!(
                "mcp: doppelter Server-Name '{}' in settings.toml",
                s.name
            )));
        }
    }
    Ok(servers)
}

/// Eine offene Verbindung zu einem MCP-Server samt gelisteten Tools.
pub struct McpConnection {
    server: String,
    service: Arc<Service>,
    tools: Vec<rmcp::model::Tool>,
}

impl McpConnection {
    pub fn server(&self) -> &str {
        &self.server
    }
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// Baut die Remote-Tools als `Arc<dyn Tool>`. Kollidiert ein Name mit einem bereits in
    /// `taken` vorhandenen, wird `<server>__<tool>` verwendet; vergebene Namen landen in `taken`.
    pub fn into_tools(self, taken: &mut HashSet<String>) -> Vec<Arc<dyn Tool>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for t in self.tools {
            let raw = t.name.to_string();
            let exposed = resolve_name(taken, &self.server, &raw);
            taken.insert(exposed.clone());
            let parameters = Value::Object((*t.input_schema).clone());
            let description = t.description.map(|d| d.to_string()).unwrap_or_default();
            out.push(Arc::new(McpTool {
                service: self.service.clone(),
                remote_name: raw,
                call_timeout: DEFAULT_CALL_TIMEOUT,
                spec: ToolSpec {
                    name: exposed.clone(),
                    label: exposed,
                    description,
                    parameters,
                },
            }));
        }
        out
    }
}

/// Wählt einen **eindeutigen** exponierten Tool-Namen: roh, falls frei; sonst `<server>__<tool>`;
/// falls auch das belegt ist, mit numerischem Suffix (`…_2`, `…_3`). Garantiert ein Ergebnis,
/// das nicht in `taken` enthalten ist (sonst überschreiben sich Tools im Dispatch-Map).
pub fn resolve_name(taken: &HashSet<String>, server: &str, raw: &str) -> String {
    let mut candidate = if taken.contains(raw) {
        format!("{server}__{raw}")
    } else {
        raw.to_string()
    };
    if !taken.contains(&candidate) {
        return candidate;
    }
    let base = candidate.clone();
    let mut i = 2;
    loop {
        candidate = format!("{base}_{i}");
        if !taken.contains(&candidate) {
            return candidate;
        }
        i += 1;
    }
}

/// Verbindet zu einem MCP-Server und listet seine Tools.
pub async fn connect(cfg: &McpServerConfig) -> Result<McpConnection> {
    let service: Service = match cfg.transport.as_str() {
        "http" => {
            let url = cfg.url.as_deref().ok_or_else(|| {
                SeppError::Config(format!(
                    "mcp '{}': 'url' fehlt für transport=http",
                    cfg.name
                ))
            })?;
            let transport = StreamableHttpClientTransport::from_uri(url);
            ().serve(transport)
                .await
                .map_err(|e| SeppError::Provider(format!("mcp '{}': connect: {e}", cfg.name)))?
        }
        "stdio" => {
            if cfg.command.is_empty() {
                return Err(SeppError::Config(format!(
                    "mcp '{}': 'command' fehlt für transport=stdio",
                    cfg.name
                )));
            }
            let mut command = tokio::process::Command::new(&cfg.command[0]);
            command.args(&cfg.command[1..]);
            // Subprozess gemäß deklarierten Capabilities einsperren (Default deny), BEVOR rmcp
            // ihn spawnt (Linux: Landlock; sonst Fallback + Warnung).
            let policy = cfg.capabilities.to_policy();
            sepp_policy::default_sandbox()
                .prepare(&mut command, &policy)
                .map_err(|e| SeppError::Provider(format!("mcp '{}': sandbox: {e}", cfg.name)))?;
            let transport = TokioChildProcess::new(command)
                .map_err(|e| SeppError::Provider(format!("mcp '{}': spawn: {e}", cfg.name)))?;
            ().serve(transport)
                .await
                .map_err(|e| SeppError::Provider(format!("mcp '{}': connect: {e}", cfg.name)))?
        }
        other => {
            return Err(SeppError::Config(format!(
                "mcp '{}': unbekannter transport '{other}' (erlaubt: http, stdio)",
                cfg.name
            )))
        }
    };

    let service = Arc::new(service);
    let tools = service
        .list_all_tools()
        .await
        .map_err(|e| SeppError::Provider(format!("mcp '{}': list_tools: {e}", cfg.name)))?;
    Ok(McpConnection {
        server: cfg.name.clone(),
        service,
        tools,
    })
}

/// Ein einzelnes Remote-Tool als `sepp_tools::Tool`.
pub struct McpTool {
    service: Arc<Service>,
    remote_name: String,
    call_timeout: Duration,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn execute(
        &self,
        input: Value,
        cancel: CancellationToken,
        _on_update: Option<&(dyn Fn(ToolResult) + Send + Sync)>,
    ) -> Result<ToolResult> {
        let arguments = match input {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                return Err(SeppError::Tool(format!(
                    "mcp '{}': Argumente müssen ein JSON-Objekt sein (war {other})",
                    self.remote_name
                )))
            }
        };
        let mut param = CallToolRequestParams::new(self.remote_name.clone());
        if let Some(args) = arguments {
            param = param.with_arguments(args);
        }

        // Aufruf zeitlich begrenzen (gegen stumme Server) und auf Cancel reagieren.
        // Hinweis: Cancel verwirft hier nur lokal; eine MCP-`CancelledNotification` an den
        // Server (sauberes Remote-Cancel) folgt mit der Policy/Sandbox-Arbeit (Phase 4).
        let result = tokio::select! {
            r = tokio::time::timeout(self.call_timeout, self.service.call_tool(param)) => match r {
                Ok(Ok(res)) => res,
                Ok(Err(e)) => {
                    return Err(SeppError::Tool(format!("mcp '{}': {e}", self.remote_name)))
                }
                Err(_) => {
                    return Err(SeppError::Tool(format!(
                        "mcp '{}': Timeout nach {} s",
                        self.remote_name,
                        self.call_timeout.as_secs()
                    )))
                }
            },
            _ = cancel.cancelled() => return Err(SeppError::Aborted),
        };

        // Content mappen: Text + Bilder; Audio/Resource (noch) ignoriert.
        let mut blocks: Vec<ContentBlock> = Vec::new();
        for c in &result.content {
            if let Some(t) = c.as_text() {
                blocks.push(ContentBlock::text(&t.text));
            } else if let Some(img) = c.as_image() {
                blocks.push(ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: img.mime_type.clone(),
                        data: img.data.clone(),
                    },
                });
            }
        }
        let details = result.structured_content.clone().unwrap_or(Value::Null);
        if blocks.is_empty() {
            // Kein darstellbarer Content → structured_content als Text, sonst Hinweis
            // (statt eines leeren Text-Blocks).
            let fallback = result
                .structured_content
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "(kein Inhalt)".into());
            blocks.push(ContentBlock::text(fallback));
        }
        // Tool-Output IMMER kürzen, bevor er ins Kontextfenster geht (MCP kürzt nicht selbst).
        let blocks = sepp_tools::truncate_content_blocks(blocks);
        Ok(ToolResult {
            content: blocks,
            details,
            is_error: result.is_error.unwrap_or(false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_name_prefixes_only_on_collision() {
        let mut taken: HashSet<String> = HashSet::new();
        taken.insert("read".into());
        assert_eq!(resolve_name(&taken, "git", "status"), "status");
        assert_eq!(resolve_name(&taken, "git", "read"), "git__read");
    }

    #[test]
    fn resolve_name_disambiguates_when_prefix_also_taken() {
        let mut taken: HashSet<String> = HashSet::new();
        taken.insert("read".into());
        taken.insert("git__read".into());
        // roh + Präfix belegt → numerisches Suffix, garantiert frei.
        let r = resolve_name(&taken, "git", "read");
        assert_eq!(r, "git__read_2");
        assert!(!taken.contains(&r));
    }

    #[test]
    fn parses_settings_servers() {
        let toml = r#"
            [[mcp.servers]]
            name = "fpv7"
            transport = "http"
            url = "https://api.fpv7.com/mcp"

            [[mcp.servers]]
            name = "git"
            transport = "stdio"
            command = ["uvx", "mcp-server-git"]
            [mcp.servers.capabilities]
            fs_read = ["./"]
            fs_write = ["./"]
            exec = ["git"]
        "#;
        let settings: Settings = toml::from_str(toml).unwrap();
        assert_eq!(settings.mcp.servers.len(), 2);
        assert_eq!(settings.mcp.servers[0].name, "fpv7");
        assert_eq!(
            settings.mcp.servers[0].url.as_deref(),
            Some("https://api.fpv7.com/mcp")
        );
        let git = &settings.mcp.servers[1];
        assert_eq!(git.transport, "stdio");
        assert_eq!(git.command, vec!["uvx", "mcp-server-git"]);
        // Capabilities → Policy: exec git erlaubt, exec rm nicht.
        let pol = git.capabilities.to_policy();
        assert!(pol.allows(&sepp_policy::Capability::Exec {
            program: "git".into()
        }));
        assert!(!pol.allows(&sepp_policy::Capability::Exec {
            program: "rm".into()
        }));
    }

    #[test]
    fn empty_settings_is_ok() {
        let settings: Settings = toml::from_str("").unwrap();
        assert!(settings.mcp.servers.is_empty());
    }
}
