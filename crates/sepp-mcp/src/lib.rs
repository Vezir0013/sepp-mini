//! `sepp-mcp` — Tier-3-Erweiterung: bindet **MCP-Server** als Tool-Quelle ein (via `rmcp`).
//!
//! Transporte: streamable HTTP und stdio (Child-Prozess). Jedes Remote-Tool wird als
//! [`McpTool`] (`impl sepp_tools::Tool`) exponiert; bei Namenskollision mit bereits
//! vergebenen Tools wird `<server>__<tool>` als Präfix genutzt. Konfiguration über
//! `[[mcp.servers]]` in `settings.toml`.
//!
//! Sandbox: stdio-Server werden vor dem Spawn per [`sepp_policy::Sandbox`] eingesperrt (Env
//! Default-deny, Landlock/Seatbelt für Dateisystem, TCP-Verbot ohne `net`, Exec-Allowlist bei
//! `exec`-Liste). Die Policy kommt entweder aus `[mcp.servers.capabilities]` ([`connect`]) oder
//! — mit Sepp Guard — bereits gemergt aus der Policy-Datei ([`connect_with_policy`]). Das stderr
//! des Servers wird gepipet und über `tracing` geloggt, statt in die TUI zu schreiben.
//!
//! **Secrets in HTTP-Headern.** `[mcp.servers.headers]` darf `$NAME`-Platzhalter enthalten; der
//! [`SecretBroker`] ersetzt sie beim Verbinden — aber nur, wenn `[mcp.<name>]` in der
//! `policy.toml` **beides** gewährt: `env = ["NAME"]` (welches Secret) und `net = ["<host>"]`
//! (wohin es darf). Fehlt eine Hälfte, bricht der Connect ab, statt einen kaputten Header zu
//! senden — ein stehengebliebener Platzhalter landete sonst im Zugriffslog des fremden Servers.
//! Zwei Entscheidungen, die dazugehören:
//!
//! * **Nur `headers`, nie `url`.** `reqwest`s Fehlertexte enthalten die URL; ein
//!   `?api_key=$TOKEN` stünde bei jedem Verbindungsfehler im Klartext in der Meldung.
//! * **Redaction deckt Fehlertexte und stdio-stderr, nicht Tool-Ergebnisse.** Ein Server, der
//!   den Auth-Header in seine *Antwort* spiegelt, schreibt das Secret damit ins Kontextfenster
//!   und in die Session-Datei. Das ist eine bewusst getragene Grenze, keine Lücke, die niemand
//!   bemerkt hat — sie fällt mit dem Egress-Proxy.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::ServiceExt;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use sepp_core::{ContentBlock, ImageSource, Result, SeppError, ToolResult, ToolSpec};
use sepp_policy::{Policy, SecretBroker};
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
    /// `[mcp.servers.headers]` — HTTP-Header für `transport = "http"`. Werte dürfen
    /// `$NAME`-Platzhalter enthalten; ersetzt wird nur, wenn `[mcp.<name>]` in der `policy.toml`
    /// **beides** gewährt: `env = ["NAME"]` (welches Secret) und `net = ["<host der url>"]`
    /// (wohin es gehen darf). Siehe [`resolve_headers`].
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
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
    /// Für die Redaction: Ein Server, der ein Secret in eine Fehlermeldung spiegelt, soll es
    /// nicht in die TUI schreiben.
    broker: Arc<SecretBroker>,
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
                broker: self.broker.clone(),
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
    // Der entfernte Name gehört dem fremden Server — wir dürfen ihn weder ablehnen (das Werkzeug
    // verschwände kommentarlos) noch ungeprüft durchreichen: Anthropic und OpenAI lehnen alles
    // außerhalb von `[A-Za-z0-9_-]` mit 400 ab, und zwar den ganzen Request. Angepasst wird nur
    // der **exponierte** Name; aufgerufen wird der Server weiterhin unter `remote_name`.
    let raw = sepp_core::sanitize_tool_name(raw);
    let server = sepp_core::sanitize_tool_name(server);
    // Nach dem Sanieren ist alles ASCII, `truncate` schneidet also nie mitten in ein Zeichen.
    let clamp = |mut s: String| {
        s.truncate(sepp_core::MAX_TOOL_NAME_LEN);
        s
    };
    let candidate = clamp(if taken.contains(&raw) {
        format!("{server}__{raw}")
    } else {
        raw.clone()
    });
    if !taken.contains(&candidate) {
        return candidate;
    }
    // Präfix UND Suffix können die Grenze sprengen — deshalb wird die Basis so gekürzt, dass
    // das Suffix noch hineinpasst, statt am Ende blind abzuschneiden.
    let base = candidate;
    let mut i = 2;
    loop {
        let suffix = format!("_{i}");
        let mut c = base.clone();
        c.truncate(sepp_core::MAX_TOOL_NAME_LEN.saturating_sub(suffix.len()));
        c.push_str(&suffix);
        if !taken.contains(&c) {
            return c;
        }
        i += 1;
    }
}

/// Die Policy aus dem **veralteten** `[mcp.servers.capabilities]`-Block eines Servers.
///
/// Sie wird nicht mehr durchgesetzt: Rechte stehen ausschließlich in der `policy.toml` unter
/// `[mcp.<name>]`. Die Funktion bleibt, damit `sepp policy` anzeigen kann, dass in einer
/// settings.toml noch etwas steht, das nichts mehr bewirkt.
pub fn policy_from_config(cfg: &McpServerConfig) -> Policy {
    cfg.capabilities.to_policy()
}

/// Host einer http(s)-URL — lebt in `sepp-policy`, weil auch der WASM-Host (`host_http`) ihn
/// braucht; hier nur re-exportiert.
pub use sepp_policy::url_host;

/// Baut den Broker für einen Server: genau die Secrets, die seine Header **verlangen** und die
/// ihm per `Env`-Recht **gewährt** sind (`SecretBroker::from_env_for` — der einzige Ort, der
/// Secret-Werte aus der Umgebung liest).
fn broker_for(cfg: &McpServerConfig, policy: &Policy) -> SecretBroker {
    SecretBroker::from_env_for(cfg.headers.values().map(String::as_str), policy)
}

/// Löst `cfg.headers` zu fertigen HTTP-Headern auf: Platzhalter ersetzt, Gates geprüft, Werte
/// als `sensitive` markiert.
///
/// Rein (kein Env, kein I/O) — der Broker wird übergeben, damit die Gate-Logik ohne Umgebung
/// und ohne Server testbar bleibt.
///
/// **Fail-closed vor dem ersten Byte.** Bliebe ein Platzhalter stehen, ginge er über die Leitung
/// und landete im Zugriffslog des fremden Servers, und die Meldung wäre ein nacktes `HTTP 401`,
/// das nichts über die fehlende Policy-Zeile sagt. Deshalb bricht jeder ungeklärte Fall hier ab,
/// mit dem passenden `sepp policy allow` in der Meldung — und **nie** mit dem Wert darin.
pub fn resolve_headers(
    cfg: &McpServerConfig,
    host: &str,
    policy: &Policy,
    broker: &SecretBroker,
) -> Result<HashMap<HeaderName, HeaderValue>> {
    let mut out = HashMap::new();
    for (name, raw) in &cfg.headers {
        let lower = name.to_ascii_lowercase();
        // rmcp setzt diese selbst und lehnt sie zur Request-Zeit ab — hier ist es erklärbar.
        if matches!(
            lower.as_str(),
            "accept" | "mcp-session-id" | "last-event-id"
        ) {
            return Err(SeppError::Config(format!(
                "mcp '{}': Header '{name}' wird vom Transport selbst gesetzt",
                cfg.name
            )));
        }
        // Das Doppel-Gate (Net → Env → gesetzt) teilt sich der Client mit dem WASM-Host; die
        // Meldung nennt den passenden `sepp policy allow`-Befehl und nie den Wert.
        for want in sepp_policy::placeholder_names(raw) {
            if let Err(refusal) = broker.gate(want, host, policy) {
                return Err(SeppError::Config(format!(
                    "mcp '{}': Header '{name}' {}",
                    cfg.name,
                    refusal.explain(&sepp_policy::Actor::Mcp(cfg.name.clone()), want)
                )));
            }
        }
        let value = broker.substitute_for_host(raw, host, policy);
        let key = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
            SeppError::Config(format!("mcp '{}': Header-Name '{name}': {e}", cfg.name))
        })?;
        // Der Wert darf NIE in eine Meldung — der klassische Fall ist ein Zeilenumbruch aus
        // `$(cat token.txt)`, und die Fehlermeldung spiegelte dann das Secret.
        let mut value = HeaderValue::from_str(&value).map_err(|_| {
            SeppError::Config(format!(
                "mcp '{}': Header '{name}' ergibt keinen gültigen Header-Wert \
                 (Steuerzeichen im Secret?)",
                cfg.name
            ))
        })?;
        // `Debug` druckt danach nur noch "Sensitive" — der Transport-Worker leitet `Debug` ab
        // und hält die Config.
        value.set_sensitive(true);
        out.insert(key, value);
    }
    Ok(out)
}

/// Verbindet zu einem MCP-Server und listet seine Tools, **ohne** Rechte. Nur für Werkzeuge
/// ohne Policy-Kontext (`examples/probe.rs`); der Agent nutzt [`connect_with_policy`].
pub async fn connect(cfg: &McpServerConfig) -> Result<McpConnection> {
    connect_with_policy(cfg, &Policy::default()).await
}

/// Wie [`connect`], aber mit der Policy des Servers aus `policy.toml [mcp.<name>]` (Verbote
/// bereits angewendet). Für `http` ist die Policy ohne Wirkung — der Server läuft auf einem
/// fremden Rechner.
pub async fn connect_with_policy(cfg: &McpServerConfig, policy: &Policy) -> Result<McpConnection> {
    // Die einzige Stelle, die Secret-Werte aus der Umgebung holt. `McpServerConfig` (derived
    // `Debug`) hält nur Platzhalter; echte Werte leben ausschließlich hier und in als
    // `sensitive` markierten `HeaderValue`s.
    let broker = Arc::new(broker_for(cfg, policy));
    let service: Service = match cfg.transport.as_str() {
        "http" => {
            let url = cfg.url.as_deref().ok_or_else(|| {
                SeppError::Config(format!(
                    "mcp '{}': 'url' fehlt für transport=http",
                    cfg.name
                ))
            })?;
            // Header VOR dem Transportbau auflösen: Ein stehengebliebener Platzhalter ginge
            // sonst über die Leitung und landete im Zugriffslog des fremden Servers.
            let headers = if cfg.headers.is_empty() {
                HashMap::new()
            } else {
                let host = url_host(url).ok_or_else(|| {
                    SeppError::Config(format!(
                        "mcp '{}': 'url' ist keine gültige http(s)-URL: {url}",
                        cfg.name
                    ))
                })?;
                resolve_headers(cfg, &host, policy, &broker)?
            };
            let transport = StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(url).custom_headers(headers),
            );
            ().serve(transport).await.map_err(|e| {
                // Ein Nicht-2xx wird von rmcp samt Antwortkörper in den Fehlertext gepackt;
                // ein Server, der den Auth-Header spiegelt, leakte ihn sonst in die TUI.
                SeppError::Provider(format!(
                    "mcp '{}': connect: {}",
                    cfg.name,
                    broker.redact(&e.to_string())
                ))
            })?
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
            // Subprozess gemäß Policy einsperren (Default deny), BEVOR rmcp ihn spawnt
            // (Linux: Landlock, macOS: Seatbelt; sonst Fallback + Warnung).
            sepp_policy::default_sandbox()
                .prepare(&mut command, policy)
                .map_err(|e| SeppError::Provider(format!("mcp '{}': sandbox: {e}", cfg.name)))?;
            // stderr NICHT erben (würde in der TUI den Bildschirm beschreiben), sondern pipen
            // und zeilenweise über tracing loggen.
            let (transport, stderr) = TokioChildProcess::builder(command)
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| SeppError::Provider(format!("mcp '{}': spawn: {e}", cfg.name)))?;
            if let Some(stderr) = stderr {
                spawn_stderr_logger(cfg.name.clone(), stderr, broker.clone());
            }
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
    let tools = service.list_all_tools().await.map_err(|e| {
        SeppError::Provider(format!(
            "mcp '{}': list_tools: {}",
            cfg.name,
            broker.redact(&e.to_string())
        ))
    })?;
    Ok(McpConnection {
        server: cfg.name.clone(),
        service,
        tools,
        broker,
    })
}

/// Loggt das stderr eines stdio-Servers zeilenweise (`target = "mcp"`), bis EOF.
fn spawn_stderr_logger(
    server: String,
    stderr: tokio::process::ChildStderr,
    broker: Arc<SecretBroker>,
) {
    use tokio::io::AsyncBufReadExt;
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            // Der Subprozess hat das Secret per `scrub_env` legitim in der Umgebung und echot
            // es bei einem Fehler gern auf stderr — von dort ginge es direkt ins Log.
            tracing::warn!(target: "mcp", server = %server, "{}", broker.redact(&line));
        }
    });
}

/// Ein einzelnes Remote-Tool als `sepp_tools::Tool`.
pub struct McpTool {
    service: Arc<Service>,
    /// Redaction für Fehlertexte. Tool-**Ergebnisse** laufen bewusst NICHT hierdurch: siehe
    /// die Grenze in der Modul-Doku.
    broker: Arc<SecretBroker>,
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
                    return Err(SeppError::Tool(format!(
                        "mcp '{}': {}",
                        self.remote_name,
                        self.broker.redact(&e.to_string())
                    )))
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

    use sepp_policy::Capability;

    fn http_cfg(headers: &[(&str, &str)]) -> McpServerConfig {
        McpServerConfig {
            name: "gh".into(),
            transport: "http".into(),
            url: Some("https://api.example.com/mcp".into()),
            command: vec![],
            capabilities: Default::default(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn pol(caps: Vec<Capability>) -> Policy {
        Policy::new(caps)
    }

    fn full_grant() -> Policy {
        pol(vec![
            Capability::Net {
                host: "api.example.com".into(),
            },
            Capability::Env {
                name: "TOKEN".into(),
            },
        ])
    }

    fn broker() -> SecretBroker {
        SecretBroker::new().with_secret("TOKEN", "sk-geheim")
    }

    #[test]
    fn headers_resolve_when_both_gates_are_open() {
        let cfg = http_cfg(&[("Authorization", "Bearer $TOKEN")]);
        let out = resolve_headers(&cfg, "api.example.com", &full_grant(), &broker()).unwrap();
        let v = out.get(&HeaderName::from_static("authorization")).unwrap();
        assert_eq!(v.to_str().unwrap(), "Bearer sk-geheim");
        // Die Nicht-Leak-Zusage: `Debug` darf den Wert nicht zeigen — der Transport-Worker
        // leitet `Debug` ab und hält die Config.
        assert_eq!(format!("{v:?}"), "Sensitive");
    }

    #[test]
    fn wildcard_net_grant_is_enough() {
        let cfg = http_cfg(&[("X-Key", "$TOKEN")]);
        let grant = pol(vec![
            Capability::Net { host: "*".into() },
            Capability::Env {
                name: "TOKEN".into(),
            },
        ]);
        assert!(resolve_headers(&cfg, "api.example.com", &grant, &broker()).is_ok());
    }

    #[test]
    fn headers_without_placeholders_need_no_grant() {
        let cfg = http_cfg(&[("X-Client", "sepp")]);
        let out = resolve_headers(&cfg, "api.example.com", &Policy::default(), &broker()).unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn missing_net_grant_names_the_host_and_the_command() {
        let cfg = http_cfg(&[("Authorization", "Bearer $TOKEN")]);
        let grant = pol(vec![Capability::Env {
            name: "TOKEN".into(),
        }]);
        let e = resolve_headers(&cfg, "api.example.com", &grant, &broker()).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("api.example.com"), "{msg}");
        assert!(msg.contains("sepp policy allow mcp.gh net"), "{msg}");
        assert!(
            !msg.contains("sk-geheim"),
            "der Wert darf nie in die Meldung: {msg}"
        );
    }

    #[test]
    fn missing_env_grant_names_the_variable_and_the_command() {
        let cfg = http_cfg(&[("Authorization", "Bearer $TOKEN")]);
        let grant = pol(vec![Capability::Net {
            host: "api.example.com".into(),
        }]);
        let msg = resolve_headers(&cfg, "api.example.com", &grant, &broker())
            .unwrap_err()
            .to_string();
        assert!(msg.contains("sepp policy allow mcp.gh env TOKEN"), "{msg}");
    }

    #[test]
    fn granted_but_unset_variable_is_reported_not_silently_left_standing() {
        let cfg = http_cfg(&[("Authorization", "Bearer $TOKEN")]);
        let empty = SecretBroker::new();
        let msg = resolve_headers(&cfg, "api.example.com", &full_grant(), &empty)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("nicht gesetzt"), "{msg}");
    }

    #[test]
    fn reserved_header_names_are_refused_at_config_time() {
        let cfg = http_cfg(&[("Accept", "text/plain")]);
        let msg = resolve_headers(&cfg, "api.example.com", &full_grant(), &broker())
            .unwrap_err()
            .to_string();
        assert!(msg.contains("vom Transport selbst gesetzt"), "{msg}");
    }

    #[test]
    fn a_secret_with_control_characters_is_refused_without_echoing_it() {
        let cfg = http_cfg(&[("Authorization", "Bearer $TOKEN")]);
        let b = SecretBroker::new().with_secret("TOKEN", "sk-mit\nZeilenumbruch");
        let msg = resolve_headers(&cfg, "api.example.com", &full_grant(), &b)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("Steuerzeichen"), "{msg}");
        assert!(!msg.contains("sk-mit"), "{msg}");
    }

    #[test]
    fn broker_loads_only_granted_and_actually_used_variables() {
        std::env::set_var("SEPP_TEST_GRANTED", "wert-a");
        std::env::set_var("SEPP_TEST_UNGRANTED", "wert-b");
        let mut cfg = http_cfg(&[("A", "$SEPP_TEST_GRANTED"), ("B", "$SEPP_TEST_UNGRANTED")]);
        cfg.name = "t".into();
        let grant = pol(vec![Capability::Env {
            name: "SEPP_TEST_GRANTED".into(),
        }]);
        let b = broker_for(&cfg, &grant);
        assert!(b.knows("SEPP_TEST_GRANTED"));
        assert!(!b.knows("SEPP_TEST_UNGRANTED"), "ohne env-Recht kein Wert");
    }

    #[test]
    fn settings_without_headers_still_parse() {
        let toml = r#"
            [[mcp.servers]]
            name = "alt"
            transport = "http"
            url = "https://api.example.com/mcp"
        "#;
        let s: Settings = toml::from_str(toml).unwrap();
        assert!(s.mcp.servers[0].headers.is_empty());
    }

    #[test]
    fn settings_with_headers_parse() {
        let toml = r#"
            [[mcp.servers]]
            name = "gh"
            transport = "http"
            url = "https://api.example.com/mcp"
            [mcp.servers.headers]
            Authorization = "Bearer $TOKEN"
        "#;
        let s: Settings = toml::from_str(toml).unwrap();
        assert_eq!(s.mcp.servers[0].headers["Authorization"], "Bearer $TOKEN");
    }

    #[test]
    fn resolve_name_prefixes_only_on_collision() {
        let mut taken: HashSet<String> = HashSet::new();
        taken.insert("read".into());
        assert_eq!(resolve_name(&taken, "git", "status"), "status");
        assert_eq!(resolve_name(&taken, "git", "read"), "git__read");
    }

    #[test]
    fn resolve_name_always_yields_a_provider_valid_name() {
        // Ein fremder Server darf keinen Namen liefern, den die API mit 400 ablehnt — sonst
        // scheitert nicht ein Werkzeug, sondern jeder Turn.
        let taken: HashSet<String> = HashSet::new();
        for raw in [
            "rp:pdf_extract",
            "mit leerzeichen",
            "grüße",
            "",
            &"x".repeat(200),
        ] {
            let r = resolve_name(&taken, "git", raw);
            assert!(sepp_core::is_valid_tool_name(&r), "{raw} -> {r}");
        }
        assert_eq!(
            resolve_name(&taken, "git", "rp:pdf_extract"),
            "rp_pdf_extract"
        );
    }

    #[test]
    fn resolve_name_stays_within_the_length_limit() {
        let long = "y".repeat(200);
        let mut taken: HashSet<String> = HashSet::new();
        // Erst den rohen, dann den Präfix-Namen belegen, damit der Suffix-Zweig greift.
        for _ in 0..4 {
            let r = resolve_name(&taken, &"s".repeat(80), &long);
            assert!(sepp_core::is_valid_tool_name(&r), "{r}");
            assert!(!taken.contains(&r));
            taken.insert(r);
        }
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

    /// Der veraltete Block wird noch geparst — nur damit `sepp policy` melden kann, dass dort
    /// etwas steht, das nicht mehr wirkt. Durchgesetzt wird er nirgends.
    #[test]
    fn legacy_capabilities_are_still_parsed_for_display() {
        let toml = r#"
            [[mcp.servers]]
            name = "git"
            transport = "stdio"
            command = ["git-mcp"]
            [mcp.servers.capabilities]
            fs_write = ["/abs/repo"]
            net = ["api.example.com"]
        "#;
        let settings: Settings = toml::from_str(toml).unwrap();
        let pol = policy_from_config(&settings.mcp.servers[0]);
        assert!(pol.net_allowed());
        assert!(pol.allows(&sepp_policy::Capability::FsWrite {
            prefix: "/abs/repo/x".into()
        }));
        assert_eq!(pol.exec_programs(), None);
    }

    #[test]
    fn empty_settings_is_ok() {
        let settings: Settings = toml::from_str("").unwrap();
        assert!(settings.mcp.servers.is_empty());
    }
}
