<div align="center">

<img src="assets/kionova-logo.png" alt="sepp mini" width="150" height="150">

<h1>sepp mini</h1>

<p><em>„Etwas in deinem Terminal ist gerade aufgewacht."</em></p>

<p><strong>Ein leichtgewichtiger, erweiterbarer Agent-Harness in Rust — eine statische Binary,<br>
kein Ballast. Sicher by default: Erweiterungen bekommen nur die Rechte, die sie<br>
deklarieren — vom Kern auf OS-Ebene erzwungen.</strong></p>

<p>
  <a href="https://github.com/Vezir0013/sepp-mini/actions/workflows/ci.yml"><img src="https://github.com/Vezir0013/sepp-mini/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-PolyForm%20Noncommercial%201.0.0-blue.svg" alt="License: PolyForm Noncommercial 1.0.0"></a>
  <img src="https://img.shields.io/badge/rust-stable-orange.svg" alt="Rust">
</p>

<sub>Ein Projekt von <strong>KIONOVA®</strong></sub>

</div>

---

`sepp mini` führt einen LLM-Agenten-Loop (Streaming, paralleler Tool-Dispatch, Compaction) mit
eingebauten Tools (`read`/`write`/`edit`/`bash`) und vier Erweiterungs-Tiers aus — als
interaktive TUI, als One-shot-Kommando oder als JSONL-RPC zum Einbetten in andere Programme.

> **Status:** v0.1 — funktional vollständig und getestet. Kernschleife, TUI, persistente
> Baum-Sessions, Erweiterbarkeit (Resources/Hooks/MCP/WASM), Sicherheits-Sandbox, native
> Sub-Agenten, Multi-Provider und Distribution sind umgesetzt. Offen: OpenTelemetry-Export und
> OAuth-Login (siehe [Roadmap](#roadmap)).

---

## Highlights

- 🔒 **Sandbox-by-default (das Alleinstellungsmerkmal).** Default ist **deny**. Code-tragende
  Erweiterungen deklarieren Capabilities (`FsRead`/`FsWrite`/`Net`/`Env`/`Exec`); der Kern parst
  sie zu einer Policy und erzwingt sie an der Grenze — Linux via **Landlock**, macOS via
  **Seatbelt**, plus Environment-Scrubbing (Subprozesse sehen keine geerbten Secrets).
- 🧩 **Vier Erweiterungs-Tiers** nach Macht/Isolation: **Resources** (Skills→System-Prompt,
  Prompt-Templates→Slash-Commands), **Hooks** (in-process Rhai), **WASM-Plugins** (memory-sandboxed,
  capability-gated, via `wasmi`), **MCP-Server** (out-of-process, OS-sandboxed).
- 🔌 **Multi-Provider hinter einem Trait:** Anthropic (Messages API) und OpenAI-kompatibel —
  inklusive lokaler Endpunkte (Ollama/vLLM) über `OPENAI_BASE_URL`, plus **`--provider mlx`** für
  lokale Apple-Silicon-Inferenz via **LM Studio** (verbindet automatisch zu `localhost:1234`).
- 🖥️ **Drei Modi, ein Kern:** interaktive **TUI**, **One-shot** (`-p`) und **JSONL-RPC** (`--rpc`).
- 🌳 **Robuste Sessions:** baumstrukturiert mit Branching und Compaction, persistent als JSONL
  (Default) oder optional **SQLite** (`--features sqlite`).
- 🤖 **Native Sub-Agenten:** delegieren Teilaufgaben in isoliertem Kontext, nur das Ergebnis
  kehrt zurück — der Wurzel-Kontext bleibt schlank.
- 🪶 **Leichtgewichtig:** eine statische Binary, Cold-Start im Millisekundenbereich, kein
  `node_modules`. Ideal für CLI, CI, Skripting und Embedding.

## Installation

### Vorgebaute statische Binary (empfohlen)

Der Installer lädt die passende Binary aus den GitHub-Releases und legt sie nach `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/Vezir0013/sepp-mini/main/install.sh | sh
```

Liegt `~/.local/bin` nicht im `PATH`, einmalig ergänzen:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc && source ~/.bashrc
```

Installation prüfen:

```bash
sepp --version
```

Unterstützte Plattformen: Linux (`x86_64`, `aarch64`, statisch via musl) und macOS (`x86_64`,
`aarch64`). Auf anderen Systemen weicht der Installer mit `sh install.sh --from-source` auf den
Quellcode-Build aus.

### Vorgebaute Binary für macOS (empfohlen)

Dieser **arch-übergreifende** Befehl lädt die passende Binary — Apple Silicon (`arm64`) **und**
Intel (`x86_64`) — und legt sie nach `/usr/local/bin` (liegt bereits im `PATH`):

```bash
ARCH=$([ "$(uname -m)" = "arm64" ] && echo aarch64 || echo x86_64)
curl -fL "https://github.com/Vezir0013/sepp-mini/releases/latest/download/sepp-${ARCH}-apple-darwin" -o /tmp/sepp
chmod +x /tmp/sepp
sudo mkdir -p /usr/local/bin
sudo mv /tmp/sepp /usr/local/bin/sepp
```

Installation prüfen:

```bash
sepp --version
```

### Lokale Modelle auf macOS — MLX via LM Studio (empfohlen)

sepp führt die Inferenz nicht selbst aus; die **MLX-Infrastruktur stellst du über
[LM Studio](https://lmstudio.ai) bereit** (Apple-Silicon-nativ, spürbar schneller als
llama.cpp/Ollama). sepp und LM Studio werden **getrennt** installiert:

1. **LM Studio installieren** und öffnen.
2. **MLX-Runtime** aktiv lassen und ein **tool-fähiges Modell deiner Wahl** laden (sepp gibt kein
   Modell vor — wichtig ist nur Function-/Tool-Calling-Fähigkeit).
3. **Local Server starten:** Developer → *Start Server* (Port **1234**).
4. sepp verbindet sich **automatisch** — kein API-Key, kein `OPENAI_BASE_URL` nötig:

```bash
sepp --provider mlx -m <in-lm-studio-geladenes-modell> -p "Was liegt in diesem Verzeichnis?"
```

`--provider mlx` zielt ohne weitere Konfiguration auf `http://localhost:1234/v1`. Läuft der Server
nicht, bricht sepp mit einer klaren Anleitung ab statt mit einem rohen Verbindungsfehler. Ein
abweichender Endpunkt/Port lässt sich per `OPENAI_BASE_URL` setzen; `-m` muss dem in LM Studio
geladenen Modell entsprechen (Identifier via `GET http://localhost:1234/v1/models`).

> **Key-Verhalten (Sicherheit):** Im Zero-Config-Fall sendet `--provider mlx` **keinen**
> `Authorization`-Header — ein für andere Tools exportierter `OPENAI_API_KEY` geht also nie an
> den lokalen Port 1234. Erst mit explizit gesetztem `OPENAI_BASE_URL` (bewusstes Opt-in, z. B.
> für einen LM-Studio-Server mit aktivierter Auth) wird ein vorhandener `OPENAI_API_KEY`
> mitgesendet.

### Vorgebaute Binary für Linux ARM (aarch64)

Für ARM64-Linux (Raspberry Pi OS 64-bit, ARM-VPS/Cloud, ARM-SBCs). Die Binary ist
statisch via musl gelinkt — keine Systemabhängigkeiten:

```bash
curl -fL "https://github.com/Vezir0013/sepp-mini/releases/latest/download/sepp-aarch64-unknown-linux-musl" -o /tmp/sepp
chmod +x /tmp/sepp
mkdir -p ~/.local/bin
mv /tmp/sepp ~/.local/bin/sepp
```

Liegt `~/.local/bin` nicht im `PATH`, einmalig ergänzen:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc && source ~/.bashrc
```

Installation prüfen:

```bash
sepp --version
```

### Mit Cargo

```bash
cargo install --git https://github.com/Vezir0013/sepp-mini --features sqlite sepp-cli
```

### Selbst bauen

```bash
git clone https://github.com/Vezir0013/sepp-mini
cd sepp-mini
cargo build --release -p sepp-cli --features sqlite
# Binary: target/release/sepp
```

## Deinstallation

Direkt aus der installierten Binary:

```bash
sepp uninstall            # entfernt die Binary; alle .sepp-Daten bleiben erhalten
sepp uninstall --purge    # entfernt zusätzlich config- und state-Root + projektlokale .sepp (Trust-Registry)
```

Alternativ über den Installer (z. B. wenn die Binary schon weg ist) — `install.sh` liegt nach
einer `curl`-Installation nicht lokal vor, daher erneut durch die Pipe:

```bash
curl -fsSL https://raw.githubusercontent.com/Vezir0013/sepp-mini/main/install.sh | sh -s -- --uninstall
# mit zusätzlichem --purge auch ~/.sepp löschen:
# … | sh -s -- --uninstall --purge
```

Oder vollständig von Hand:

```bash
rm ~/.local/bin/sepp      # bzw. /usr/local/bin/sepp (macOS-Installationsweg)
rm -rf ~/.sepp            # nur, falls Sessions + Config ebenfalls entfernt werden sollen
```

## Schnellstart

```bash
export ANTHROPIC_API_KEY=...          # für Anthropic-Aufrufe

sepp -p "Fasse die Datei README.md zusammen"   # One-shot (Ausgabe nach stdout)
sepp                                            # interaktive TUI
sepp -c                                         # TUI, jüngste Session fortsetzen
echo '{"type":"prompt","text":"hallo"}' | sepp --rpc   # JSONL-RPC

# OpenAI-kompatibel / lokal:
export OPENAI_API_KEY=...
sepp --provider openai -m gpt-4o-mini -p "..."
# --provider local braucht OPENAI_BASE_URL (kein stiller Cloud-Fallback):
OPENAI_BASE_URL=http://localhost:11434/v1 sepp --provider local -m llama3 -p "..."
```

Wichtige Optionen: `-p/--print`, `-c/--continue`, `-r/--resume [id]`, `-m/--model`,
`--max-tokens`, `--provider anthropic|openai|local|zai|mlx`, `--rpc`, `--sqlite`.
`sepp --help` zeigt alles.

> Im RPC- und One-shot-Modus ist **stdout der reine Datenkanal**; alle Logs gehen nach stderr.

## Konfiguration

| Variable | Zweck |
|----------|-------|
| `ANTHROPIC_API_KEY` | Anthropic-Live-Aufrufe |
| `OPENAI_API_KEY` | OpenAI (optional bei lokalen Servern; `--provider mlx` sendet ihn nur bei explizit gesetztem `OPENAI_BASE_URL`) |
| `OPENAI_BASE_URL` | OpenAI-kompatible base_url (Ollama/vLLM/local/mlx); Pflicht für `--provider local` |
| `ZAI_API_KEY` | z.ai/Zhipu-GLM (Pflicht für `--provider zai`) |
| `ZAI_BASE_URL` | z.ai base_url überschreiben (Default api.z.ai) |
| `SEPP_PROVIDER` | Default-Provider, wenn `--provider` fehlt |
| `SEPP_THINK` | Default-Reasoning (on/off), wenn `--think`/`--no-think` fehlt |
| `RUST_LOG` | Log-Level (One-shot/RPC; Logs nach stderr) |

Standardmäßig liegt alles unter der einen Wurzel `~/.sepp/`. Für System-Installationen ist die Wurzel
**FHS-fähig** getrennt in **config_root** (`settings.toml`, `skills/`, `prompts/`, `hooks/`,
`plugins/`; via `$SEPP_CONFIG_DIR`, Default `/etc/sepp` im Systemfall) und **state_root** (`sessions/`,
`trust.json`; via `$SEPP_STATE_DIR`, Default `/var/lib/sepp`). `SEPP_HOME` setzt beide zugleich.
Projektlokale **Config**-Erweiterungen (`<repo>/.sepp/…`, nur skills/prompts/hooks/plugins/settings)
laden erst, nachdem das Projekt **getrustet** wurde; Sessions/Trust liegen zentral im state_root.

**Erstkonfiguration:** `sepp init` legt das projektlokale Config-Skelett
`./.sepp/{skills,prompts,hooks,plugins}/` samt kommentierter Beispiel-`settings.toml` an;
`sepp init --global` zielt auf `~/.sepp`, `sepp init --system` legt das FHS-Layout
(`/etc/sepp` + `/var/lib/sepp`) in einem Befehl an. Der Befehl ist idempotent — vorhandene Dateien und
Verzeichnisse bleiben unangetastet.

## Erweiterungen

| Tier | Was | Wie |
|------|-----|-----|
| **Resources** | Skills (→ System-Prompt), Prompt-Templates (→ `/commands`), Themes | Dateien unter `~/.sepp/skills` · `~/.sepp/prompts` |
| **Hooks** | In-process Rhai-Skripte, die den Loop unterbrechen können | `~/.sepp/hooks/*.rhai` |
| **WASM** | Capability-gegatete Plugins (jede Sprache → `*.wasm`), Ressourcen-Limits via `[limits]` | `~/.sepp/plugins/*.wasm` + `manifest.toml` |
| **MCP** | Out-of-process-Server als Tool-Quelle (OS-sandboxed) | `~/.sepp/settings.toml` → `[[mcp.servers]]` |

Beispiel `settings.toml` (MCP-Server mit deklarierten Capabilities):

```toml
[[mcp.servers]]
name = "git"
transport = "stdio"
command = ["uvx", "mcp-server-git"]
[mcp.servers.capabilities]
fs_read  = ["./"]
fs_write = ["./"]
exec     = ["git"]
```

Beispiel `manifest.toml` (WASM-Plugin mit Capabilities und Ressourcen-Limits):

```toml
name  = "string-tools"
kind  = "wasm"
entry = "string_tools.wasm"

[capabilities]
fs_read = ["/data"]

[limits]                    # optional; fehlend = konservative Defaults
max_memory_pages = 256      # 1 Page = 64 KiB → 16 MiB
max_wall_time_ms = 30000    # Wanduhr-Budget pro Tool-Aufruf; 0 = unbegrenzt, aber unterbrechbar
fuel_slice       = 1000000  # Instruktionen pro Zeitscheibe (Yield-Intervall)
```

## Sicherheitsmodell

Default ist **deny**. Eine Erweiterung bekommt nur die Rechte, die sie deklariert und der Mensch
bestätigt — und der Kern erzwingt sie an der jeweiligen Grenze:

- **MCP/Subprozesse:** OS-Dateisystem-Sandbox — Linux via **Landlock**, macOS via **Seatbelt**
  (`sandbox_init`) — plus Environment-Scrubbing (nur gewährte `Env`-Vars + minimale Allowlist;
  **keine** geerbten API-Keys). Lässt sich die Sandbox nicht durchsetzen (Kernel ohne Landlock,
  `sandbox_init`-Fehler), wird **fail-closed** verfahren. Auf Plattformen ohne Adapter
  (Windows/BSD) gibt es kein FS-Sandboxing — nur Env-Scrubbing, mit deutlicher Warnung.
- **WASM:** Host-Funktionen werden nur registriert, wenn die Policy sie erlaubt — ein Plugin ohne
  `Net` kann nachweislich nicht ins Netz. Neben Zugriff ist auch **Verbrauch** gedeckelt:
  CPU via Fuel-Slicing (die Ausführung yieldet regelmäßig an den Host und ist damit jederzeit
  unterbrechbar — Ctrl-C wirkt auch mitten in einer Endlosschleife), Speicher via hartem
  Page-Limit (`memory.grow` darüber liefert dem Plugin `-1`), Laufzeit via Wanduhr-Budget.
  Kein `[limits]`-Abschnitt im Manifest heißt konservative Defaults, nicht „unbegrenzt".
- **Secrets:** API-Keys kommen aus Env-Vars, werden nie geloggt/persistiert; das `bash`-Tool
  reicht sie nicht an Shell-Kommandos durch.
- **Tool-Output** ist immer getrunkt, bevor er ins Kontextfenster geht.

Schwachstellen melden: [`SECURITY.md`](./SECURITY.md).

## Architektur

Cargo-Workspace aus kleinen Crates mit strikten Schichtgrenzen (untere Crates importieren nie obere):

```
sepp-core      Typen + reine Logik (kein I/O, kein tokio)
  ├── sepp-provider   Provider-Trait + Anthropic/OpenAI (HTTP/SSE)
  ├── sepp-tools      built-in Tools read/write/edit/bash + Truncation
  ├── sepp-session    Baum-Sessions (JSONL, optional SQLite)
  └── sepp-policy     Capabilities / Policy / Sandbox (Landlock) / Secret-Broker
        ├── sepp-hooks  Rhai-Hook-Bus
        ├── sepp-wasm   WASM-Plugin-Host (wasmi)
        └── sepp-mcp    MCP-Client als Tool-Quelle
sepp-agent     Agent-Loop, Tool-Dispatch, Budget, Sub-Agenten (bindet alle sepp-*)
sepp-cli       Frontends: TUI / One-shot / RPC
```

## Entwicklung

[`just`](https://github.com/casey/just) ist der Task-Runner; ohne `just` die `cargo`-Kommandos
direkt nutzen.

```bash
just check          # DAS Tor: fmt --check + clippy -D warnings + tests
just build          # cargo build --workspace
just test           # Tests (nextest, sonst cargo test)
just audit          # cargo audit + cargo deny check
just run -- -p "hi" # CLI ausführen
```

Konventionen: kleine grüne Schritte, Conventional Commits, exakt gepinnte Dependencies,
keine `unwrap`/`expect`/`panic` in Library-Crates. Siehe [`CONTRIBUTING.md`](./CONTRIBUTING.md).
Reine Code-Arbeit braucht keinen API-Key (Live-LLM-Tests sind per Default geskippt).

## Roadmap

- [ ] OpenTelemetry-Export (`tracing`), optional aktivierbar
- [ ] OAuth-Login für Subscription-Provider
- [ ] Google-Provider-Adapter
- [ ] Netz-Sandbox für MCP-Subprozesse (seccomp/Namespaces; Landlock deckt aktuell nur das Dateisystem ab)

## Mitwirken

Beiträge sind willkommen — siehe [`CONTRIBUTING.md`](./CONTRIBUTING.md) und den
[Code of Conduct](./CODE_OF_CONDUCT.md). Issues und PRs bitte über GitHub.

## Lizenz

Lizenziert unter der [PolyForm Noncommercial License 1.0.0](./LICENSE) — eine
**source-available**-Lizenz, die **ausschließlich nicht-kommerzielle Nutzung** erlaubt. Der
Patent-Grant gilt nur für diese erlaubte (nicht-kommerzielle) Nutzung; ein kommerzieller
Patent-Grant wird nicht gewährt. Für kommerzielle Nutzung bitte den Autor kontaktieren. Sofern
nicht anders angegeben, werden beigesteuerte Beiträge unter denselben Bedingungen aufgenommen.
