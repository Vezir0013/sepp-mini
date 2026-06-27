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
  sie zu einer Policy und erzwingt sie an der Grenze — Linux via **Landlock**, plus
  Environment-Scrubbing (Subprozesse sehen keine geerbten Secrets).
- 🧩 **Vier Erweiterungs-Tiers** nach Macht/Isolation: **Resources** (Skills→System-Prompt,
  Prompt-Templates→Slash-Commands), **Hooks** (in-process Rhai), **WASM-Plugins** (memory-sandboxed,
  capability-gated, via `wasmi`), **MCP-Server** (out-of-process, OS-sandboxed).
- 🔌 **Multi-Provider hinter einem Trait:** Anthropic (Messages API) und OpenAI-kompatibel —
  inklusive lokaler Endpunkte (Ollama/vLLM) über `OPENAI_BASE_URL`.
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

Installation prüfen:

```bash
sepp --version
```

Unterstützte Plattformen: Linux (`x86_64`, `aarch64`, statisch via musl) und macOS (`x86_64`,
`aarch64`). Auf anderen Systemen weicht der Installer mit `sh install.sh --from-source` auf den
Quellcode-Build aus.

### Vorgebaute Binary für macOS (empfohlen)

Falls der allgemeine Installer auf macOS abbricht (z. B. ein Schreibfehler beim Ziel), installiert
dieser **arch-übergreifende** Befehl die passende Binary direkt — Apple Silicon (`arm64`) **und**
Intel (`x86_64`):

```bash
mkdir -p ~/.local/bin
ARCH=$([ "$(uname -m)" = "arm64" ] && echo aarch64 || echo x86_64)
curl -fL "https://github.com/Vezir0013/sepp-mini/releases/latest/download/sepp-${ARCH}-apple-darwin" \
  -o ~/.local/bin/sepp
chmod +x ~/.local/bin/sepp
~/.local/bin/sepp --version
```

Liegt `~/.local/bin` nicht im `PATH`, ergänze in `~/.zshrc`:
`export PATH="$HOME/.local/bin:$PATH"`.

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
OPENAI_BASE_URL=http://localhost:11434/v1 sepp --provider local -m llama3 -p "..."
```

Wichtige Optionen: `-p/--print`, `-c/--continue`, `-r/--resume [id]`, `-m/--model`,
`--max-tokens`, `--provider anthropic|openai|local`, `--rpc`, `--sqlite`. `sepp --help` zeigt alles.

> Im RPC- und One-shot-Modus ist **stdout der reine Datenkanal**; alle Logs gehen nach stderr.

## Konfiguration

| Variable | Zweck |
|----------|-------|
| `ANTHROPIC_API_KEY` | Anthropic-Live-Aufrufe |
| `OPENAI_API_KEY` | OpenAI (optional bei lokalen Servern) |
| `OPENAI_BASE_URL` | OpenAI-kompatible base_url (Ollama/vLLM/local) |
| `SEPP_PROVIDER` | Default-Provider, wenn `--provider` fehlt |
| `RUST_LOG` | Log-Level (One-shot/RPC; Logs nach stderr) |

Globale Konfiguration und Erweiterungen liegen unter `~/.sepp/` (Sessions, `settings.toml` für
MCP-Server, `plugins/` für WASM, `skills/`, `prompts/`, `hooks/`). Projektlokale Erweiterungen
(`<repo>/.sepp/…`) laden erst, nachdem das Projekt **getrustet** wurde.

## Erweiterungen

| Tier | Was | Wie |
|------|-----|-----|
| **Resources** | Skills (→ System-Prompt), Prompt-Templates (→ `/commands`), Themes | Dateien unter `~/.sepp/skills` · `~/.sepp/prompts` |
| **Hooks** | In-process Rhai-Skripte, die den Loop unterbrechen können | `~/.sepp/hooks/*.rhai` |
| **WASM** | Capability-gegatete Plugins (jede Sprache → `*.wasm`) | `~/.sepp/plugins/*.wasm` + `manifest.toml` |
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

## Sicherheitsmodell

Default ist **deny**. Eine Erweiterung bekommt nur die Rechte, die sie deklariert und der Mensch
bestätigt — und der Kern erzwingt sie an der jeweiligen Grenze:

- **MCP/Subprozesse:** OS-Sandbox via Landlock (Dateisystem) + Environment-Scrubbing (nur
  gewährte `Env`-Vars + minimale Allowlist; **keine** geerbten API-Keys). Auf Kerneln ohne
  durchsetzbares Landlock wird **fail-closed** verfahren.
- **WASM:** Host-Funktionen werden nur registriert, wenn die Policy sie erlaubt — ein Plugin ohne
  `Net` kann nachweislich nicht ins Netz.
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
