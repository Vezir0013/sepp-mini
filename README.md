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

---

## Highlights

- 🔒 **Sandbox-by-default (das Alleinstellungsmerkmal).** Default ist **deny**. Code-tragende
  Erweiterungen deklarieren Capabilities (`FsRead`/`FsWrite`/`Net`/`Env`/`Exec`); der Kern parst
  sie zu einer Policy und erzwingt sie an der Grenze — Linux via **Landlock**, macOS via
  **Seatbelt**, plus Environment-Scrubbing (Subprozesse sehen keine geerbten Secrets).
- 🛡️ **Sepp Guard: auch der Agent selbst ist eingesperrt.** `bash` läuft in derselben OS-Sandbox
  (Projekt und `$TMPDIR` schreibbar, Systempfade lesbar, kein Netz), `read`/`write`/`edit` prüfen
  Pfade gegen dieselbe Policy. Ein Regelwerk (`policy.toml`), ein Entscheider, und `sepp policy`
  zeigt, wer was darf und wer es durchsetzt. `sepp audit` liest die Spur nach: jede Entscheidung,
  jeder Tool-Aufruf, jede Delegation (siehe [Sicherheitsmodell](#sicherheitsmodell)).
- 🧩 **Vier Erweiterungs-Tiers** nach Macht/Isolation: **Resources** (Skills→System-Prompt,
  Prompt-Templates→Slash-Commands), **Hooks** (in-process Rhai), **WASM-Plugins** (memory-sandboxed,
  capability-gated, via `wasmi`), **MCP-Server** (out-of-process, OS-sandboxed).
- 🔌 **Multi-Provider hinter einem Trait:** Anthropic (Messages API) und OpenAI-kompatibel —
  dedizierte Connector für **z.ai/Zhipu-GLM** und **Moonshot AI/Kimi** (`--provider moonshot`,
  Kimi K3 mit 1M-Kontext), lokale Endpunkte (Ollama/vLLM) über `OPENAI_BASE_URL`, plus
  **`--provider mlx`** für lokale Apple-Silicon-Inferenz via **LM Studio** (verbindet automatisch
  zu `localhost:1234`).
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

# Moonshot AI / Kimi:
export MOONSHOT_API_KEY=...
sepp -m kimi-k3 -p "..."               # Provider wird aus dem Modell abgeleitet
sepp --provider moonshot -p "..."      # Default-Modell kimi-k3
```

Wichtige Optionen: `-p/--print`, `-c/--continue`, `-r/--resume [id]`, `-m/--model`,
`--max-tokens`, `--provider anthropic|openai|local|zai|moonshot|mlx`, `--mode ask|auto|yolo`,
`--rpc`, `--sqlite`.
`sepp --help` zeigt alles.

> **Reasoning bei Moonshot:** Kimi denkt immer — die API kennt kein Abschalten, nur die Stufen
> `low|high|max`. `--no-think` senkt dort also nur den Aufwand, statt Reasoning auszuschalten
> (sepp weist beim Start darauf hin). Weil das Denken gegen dasselbe Output-Budget zählt, ist der
> `--max-tokens`-Default für Moonshot-Modelle 32768 statt 8192. Kimi K3 bringt 1M Kontext mit;
> die Auto-Compaction verdichtet dennoch spätestens bei 256.000 Token, weil jeder Turn den
> gesamten Kontext erneut überträgt.

> Im RPC- und One-shot-Modus ist **stdout der reine Datenkanal**; alle Logs gehen nach stderr.

## Konfiguration

| Variable | Zweck |
|----------|-------|
| `ANTHROPIC_API_KEY` | Anthropic-Live-Aufrufe |
| `OPENAI_API_KEY` | OpenAI (optional bei lokalen Servern; `--provider mlx` sendet ihn nur bei explizit gesetztem `OPENAI_BASE_URL`) |
| `OPENAI_BASE_URL` | OpenAI-kompatible base_url (Ollama/vLLM/local/mlx); Pflicht für `--provider local` |
| `ZAI_API_KEY` | z.ai/Zhipu-GLM (Pflicht für `--provider zai`) |
| `ZAI_BASE_URL` | z.ai base_url überschreiben (Default api.z.ai) |
| `MOONSHOT_API_KEY` | Moonshot AI/Kimi (Pflicht für `--provider moonshot`) |
| `MOONSHOT_BASE_URL` | Moonshot base_url überschreiben (Default `https://api.moonshot.ai/v1`) |
| `SEPP_PROVIDER` | Default-Provider, wenn `--provider` fehlt |
| `SEPP_THINK` | Default-Reasoning (on/off), wenn `--think`/`--no-think` fehlt |
| `SEPP_MODE` | Sepp-Guard-Modus (`ask`/`auto`/`yolo`), wenn `--mode` fehlt |
| `RUST_LOG` | Log-Level (One-shot/RPC; Logs nach stderr) |

Standardmäßig liegt alles unter der einen Wurzel `~/.sepp/`. Für System-Installationen ist die Wurzel
**FHS-fähig** getrennt in **config_root** (`settings.toml`, `skills/`, `prompts/`, `hooks/`,
`plugins/`, `pkg/`; via `$SEPP_CONFIG_DIR`, Default `/etc/sepp` im Systemfall) und **state_root**
(`sessions/`, `trust.json`, `pkg/` mit Nachweisen und Schlüsseln, `trusted-keys/`; via
`$SEPP_STATE_DIR`, Default `/var/lib/sepp`). `SEPP_HOME` setzt beide zugleich.
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
| **WASM** | Capability-gegatete Plugins (jede Sprache → `*.wasm`), Ressourcen-Limits via `[limits]`; Rust-SDK `sepp-plugin` + `sepp plugin new` | `~/.sepp/plugins/*.wasm` + `manifest.toml`; [Beispiel und Anleitung](./examples/textstat-plugin/), Vertrag [`wit/sepp.wit`](./wit/sepp.wit) |
| **MCP** | Out-of-process-Server als Tool-Quelle (OS-sandboxed) | `~/.sepp/settings.toml` → `[[mcp.servers]]` |
| **Pakete** | Skills, Prompts, Hooks und Plugins gebündelt, signiert; Rechte als Zustimmung bei der Installation | `sepp pkg install <datei.seppkg>` oder `sepp pkg install <name>` aus einer Registry (`[[registries]]` in `settings.toml`) → `~/.sepp/pkg/<name>/`; Rechte als markierter Block in `policy.toml` |

Beispiel `settings.toml` (MCP-Server mit deklarierten Capabilities):

```toml
[[mcp.servers]]
name = "git"
transport = "stdio"
command = ["uvx", "mcp-server-git"]
```

Was ein Server **darf**, steht nicht hier, sondern in der `policy.toml` unter `[mcp.git]`. Die
settings.toml sagt, was läuft; das Regelwerk sagt, was es darf.

Ein entfernter Server braucht oft einen Schlüssel. Der steht als Platzhalter im Header — nie im
Klartext und nie in der `url`, denn die taucht in jeder Verbindungsfehlermeldung auf:

```toml
[[mcp.servers]]
name = "example"
transport = "http"
url = "https://mcp.example.com"

[mcp.servers.headers]
Authorization = "Bearer $EXAMPLE_TOKEN"
```

```toml
# policy.toml — beide Zeilen nötig, sonst wird gar nicht erst verbunden
[mcp.example]
net = ["mcp.example.com"]   # wohin das Secret gehen darf
env = ["EXAMPLE_TOKEN"]     # welches Secret dieser Server sehen darf
```

Der Wert kommt aus der Umgebung und wird **vor** dem Verbinden eingesetzt. Fehlt eine der beiden
Zeilen, bricht sepp mit der passenden `sepp policy allow`-Empfehlung ab, statt einen Header mit
literalem `$EXAMPLE_TOKEN` loszuschicken.

Beispiel `manifest.toml` (WASM-Plugin mit Capabilities und Ressourcen-Limits):

```toml
name  = "string-tools"
kind  = "wasm"
entry = "string_tools.wasm"

abi   = 1                   # Version des Plugin-Protokolls; fehlend = 1

[capabilities]
fs_read = ["/data"]

[limits]                    # optional; fehlend = konservative Defaults
max_memory_pages = 256      # 1 Page = 64 KiB → 16 MiB
max_wall_time_ms = 30000    # Wanduhr-Budget pro Tool-Aufruf; 0 = unbegrenzt, aber unterbrechbar
fuel_slice       = 1000000  # Instruktionen pro Zeitscheibe (Yield-Intervall)
```

### Ein Plugin schreiben

Der Autor schreibt eine Funktion, kein Protokoll. Das SDK `sepp-plugin` (Rust, Ziel
`wasm32-unknown-unknown`) kapselt Exports, Zeiger und Abholweg; das Attribut `#[sepp_plugin::tool]`
macht aus einer Funktion das Werkzeug, und das Parameter-Schema fürs Modell entsteht aus dem
`Args`-Typ:

```rust
use sepp_plugin::prelude::*;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Der zu vermessende Text.
    text: String,
}

#[sepp_plugin::tool(desc = "Zählt die Wörter eines Textes.")]
fn woerter(args: Args, host: &Host) -> Result<ToolResult> {
    host.log("los");
    let n = args.text.split_whitespace().count();
    Ok(ToolResult::text(format!("{n} Wörter")).with_details(json!({ "n": n })))
}
```

```bash
sepp plugin new woerter                 # Gerüst: Cargo.toml, src/lib.rs, Manifest, README
cd woerter && cargo test                # nativ, ohne wasm32-Target
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/woerter.wasm woerter.toml ~/.sepp/plugins/
```

Fähigkeiten sind **Cargo-Features und damit Compile-Gates**: `host.fs().read(..)` gibt es nur mit
dem Feature `fs-read`, `host.http()` nur mit `net`. Ein Feature schaltet zugleich den Host-Import
frei — das Modul importiert nur, was es benutzt, und der Host registriert eine gegatete Funktion
nur, wenn Manifest **und** `policy.toml` sie gewähren. Fehler sind ein `Err(..)`; das SDK macht
daraus ein Ergebnis mit `is_error = true`, ein Plugin trappt nie. Ein lauffähiges Beispiel samt
Anleitung liegt unter [`examples/textstat-plugin/`](./examples/textstat-plugin/).

Darunter liegt ein kleines Protokoll, seit **ABI Version 1** festgezurrt und in
[`wit/sepp.wit`](./wit/sepp.wit) als Vertrag aufgeschrieben (logische Schnittstelle als WIT, darunter
die Kodierung für Core-WASM; ein Test hält Vertrag und Host synchron). Wer ohne SDK baut — etwa in
einer anderen Sprache — exportiert `memory`, `sepp_spec`, `sepp_alloc` und `sepp_call` und bekommt
aus dem Modul `env` fünf Funktionen: `host_log` und `host_result_read` immer, `host_fs_read` und
`host_fs_read_bytes` mit dem Recht `fs_read`, `host_http` mit `net`. Es gibt bewusst kein Freigeben:
Der Host verwirft nach jedem Aufruf die ganze Instanz, ein Plugin hält keinen Zustand zwischen zwei
Aufrufen. Und die Standardbibliothek trägt für `wasm32-unknown-unknown` nur zur Hälfte — ein Modul
hat weder Uhr noch Zufall noch Dateizugriff außer über den Host.

### Ein Paket installieren

Ein Paket (`.seppkg`) bündelt Skills, Prompts, Hooks und Plugins, ist vom Herausgeber signiert und
**bringt keine Rechte mit — es bittet um sie**:

```bash
sepp pkg install rechnungspruefung-1.0.0.seppkg
```

`install` prüft die Signatur, zeigt beim ersten Paket eines Herausgebers dessen Fingerprint zur
Bestätigung (danach muss jedes weitere Paket dieses Namens zum Schlüssel passen), fragt die
Variablen des Pakets ab (etwa den Ordner mit den Belegen), listet je Plugin die beantragten Rechte
mit echten Pfaden und je Hook den Hinweis, dass er jeden Werkzeugaufruf blockieren kann — und
schreibt die Rechte erst nach Zustimmung als markierten Block in deine `policy.toml`:

```toml
# von sepp pkg: rechnungspruefung 1.0.0 — nicht von Hand ändern
[plugin.pdf_extract]
fs_read = ["/home/anna/buchhaltung/2026"]
net = ["api.example.com"]
# Ende sepp pkg: rechnungspruefung
```

Danach ist das Paket rechtlich nichts Besonderes: `sepp policy` zeigt es wie handgeschriebene
Zeilen. Die Dateien liegen unter `~/.sepp/pkg/<name>/`, deine eigenen Verzeichnisse bleiben
unangetastet. `sepp pkg list` zeigt Pakete und vertraute Herausgeber, `sepp pkg remove <name>`
nimmt Verzeichnis, Block und Nachweis wieder heraus. Nicht-interaktiv: `--yes`,
`--trust-key <fingerprint>`, `--var NAME=WERT`.

Aus einer **Registry** kommt ein Paket beim Namen. Die Registry ist ein signierter Index auf einem
beliebigen Webspace, den du einmal in `~/.sepp/settings.toml` einträgst — samt Public Key des
Betreibers, gepinnt, ohne Dialog:

```toml
[[registries]]
name = "kionova"
url = "https://pkg.example.com/index.toml"
key = "<base64, 32 Byte — nennt der Betreiber>"
```

```bash
sepp pkg search rechnung             # Index durchsuchen
sepp pkg install rechnungspruefung   # höchste Version — oder rechnungspruefung@1.0.0
sepp pkg untrust acme                # Vertrauen in einen Herausgeber zurücknehmen
```

Das Paket wird gedeckelt und gehasht geladen und geht dann exakt denselben Weg wie eine Datei:
Signatur, Fingerprint, Zustimmung — der Dialog nennt zusätzlich Registry und URL, und der
Herausgeber-Schlüssel aus dem Index muss zum Paket passen. Ein Index nennt Pakete, er gewährt
nichts. Netz gibt es nur hier: `https://` immer, `http://` nur für localhost.

### Ein Paket bauen

```bash
sepp pkg keygen                  # Schlüsselpaar unter ~/.sepp/pkg/, einmalig
sepp pkg pack rechnungspruefung  # Verzeichnis → rechnungspruefung-1.0.0.seppkg
```

Das Verzeichnis enthält `manifest.toml` und die Inhalte in denselben Unterordnern wie beim Nutzer:

```toml
format = 1
name = "rechnungspruefung"
version = "1.0.0"
description = "Eingangsrechnungen nach §14 UStG prüfen"

[publisher]
name = "acme"                    # den Schlüssel trägt `pack` ein

[vars.BELEGE_DIR]
description = "Ordner mit den Belegen"
kind = "path"

[rights.pdf_extract]             # nicht mehr, als plugins/pdf_extract.toml deklariert
fs_read = ["${BELEGE_DIR}"]
net = ["api.example.com"]
```

`pack` berechnet SHA-256 je Datei, trägt `[files]` ein, signiert das Manifest mit Ed25519 und
packt reproduzierbar (zstd-komprimiertes tar). Der Vertrag des Formats steht in
`crates/sepp-pkg/src/lib.rs`.

### Eine Registry betreiben

```bash
sepp pkg keygen --registry            # Betreiber-Schlüsselpaar: ~/.sepp/pkg/registry.key|.pub
sepp pkg index ./site --name kionova  # index.toml + index.sig neben die .seppkg-Dateien in ./site
```

`index` prüft jedes Paket wie der Installer, schreibt je Eintrag Name, Version, Herausgeber samt
Schlüssel, URL (relativ zum Index oder mit `--base-url` absolut), SHA-256 und Größe, signiert den
Index und gibt den fertigen `[[registries]]`-Eintrag aus, den Nutzer übernehmen. Ausliefern
heißt: das Verzeichnis auf einen Webspace legen — GitHub Pages, ein Release, ein Ordner hinter
nginx. Der Vertrag des Index steht in `crates/sepp-pkg/src/registry.rs`.

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

### Sepp Guard: der Agent selbst ist eingesperrt

Erweiterungen zu sandboxen reicht nicht, wenn das Modell über `bash` alles darf. Sepp Guard legt
**ein Regelwerk** über alle Akteure und lässt es von **mehreren Vollstreckern** durchsetzen:

| Akteur | Prüfung | Durchsetzung |
|---|---|---|
| `bash` | Rückfrage-Muster, Audit | OS-Sandbox mit der Agent-Policy: Environment geleert bis auf eine Allowlist, Dateisystem via Landlock (Linux) / Seatbelt (macOS), TCP verboten ohne `net`, Exec-Allowlist bei `exec`-Liste |
| `read` / `write` / `edit` | Pfadprüfung (kanonisch, auch für neue Dateien und Symlinks) | in-process |
| `task` (Sub-Agent) | erbt die Guard-Tools | wie oben |
| MCP stdio | `[mcp.<name>]`, minus Verbote | wie bisher, plus Netz/Exec; stderr des Servers landet im Log |
| MCP http | keine | keine (remote); unter `[deny] net` wird gar nicht erst verbunden |
| WASM | `[plugin.<name>]` ∩ Manifest, ohne Abschnitt nichts | wasmi-Linker-Gate |

**Defaults ohne Konfiguration:** Projekt und Systempfade lesbar, Projekt und `$TMPDIR` schreibbar,
Ausführen unbeschränkt, **kein Netz**, minimale Umgebung (`PATH HOME LANG LC_* TERM TMPDIR`),
Verbote auf `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.sepp` sowie config- und state-Root.

**Modi:** `--mode ask` (TUI-Default: außerhalb der Policy erscheint ein Dialog — einmal erlauben,
für die Sitzung, dauerhaft oder ablehnen), `auto` (Default bei `-p`/`--rpc`: innerhalb erlaubt,
außerhalb verweigert), `yolo` (keine Sandbox, bisheriges Verhalten). Ist die Sandbox nicht
durchsetzbar, startet der Agent nicht (fail-closed); der Ausweg ist ausdrücklich `--mode yolo`.

```
┌ Berechtigung — Sepp Guard ───────────────────────────────┐
│ Akteur  agent                                            │
│ Aktion  schreiben /home/du/.config/werkzeug.toml         │
│ Grund   … liegt außerhalb der Policy für agent           │
│                                                          │
│   [e]  einmal erlauben                                   │
│   [s]  für diese Sitzung erlauben                        │
│   [d]  dauerhaft erlauben (schreibt in policy.toml)      │
│   [n]  ablehnen                                          │
└──────────────────────────────────────────────────────────┘
```

„Dauerhaft" trägt das Recht selbst in die `policy.toml` ein; Kommentare und Formatierung bleiben
erhalten. Dasselbe von Hand: `sepp policy allow agent fs_write ~/.config` (mit `--global` in die
globale Datei). In der TUI zeigt `/policy` das Regelwerk.

**Regelwerk** — `.sepp/policy.toml` im Projekt (lädt nach Trust), global `~/.sepp/policy.toml` oder
`[policy]` in `settings.toml`. Einträge erweitern die Defaults, `[deny]` schränkt ein:

```toml
mode = "ask"

[agent]                          # bash, read, write, edit und task
fs_read  = ["~/.cargo", "~/.rustup"]
fs_write = ["~/.cargo"]
net      = true                  # TCP erlauben; Host-Listen wirken erst mit dem Egress-Proxy
env      = ["CARGO_HOME"]

[agent.ask]
patterns = ["rm -rf", "git push --force"]

[mcp.git]                        # die einzige Rechtequelle für diesen Server
fs_write = ["./"]
exec     = ["git"]

[plugin.string-tools]            # Gewährung; effektiv: Schnitt mit dem Manifest
net      = ["api.example.com"]

[deny]                           # gewinnt gegen jede Quelle und jeden Akteur
fs_read  = ["~/.config/secrets"]
net      = true                  # Hauptschalter: niemand kommt ins Netz
```

**Wer nichts einträgt, gewährt nichts.** Ein Manifest ist die Selbstauskunft des Plugin-Autors,
keine Grenze: Ohne `[plugin.<name>]` bekommt ein Plugin keine Rechte. Durchgesetzt wird das am
Linker — eine Host-Funktion wie `host_http` wird nur registriert, wenn das Recht gewährt ist, und
ein Modul, das sie ohne Gewährung importiert, lässt sich nicht instanziieren und lädt gar nicht
erst. Die Meldung dazu erscheint beim Start und nennt den fehlenden Abschnitt.

`sepp init` legt die Datei an und aktiviert das Preset für erkannte Projekttypen (Rust, Node,
Python). `sepp policy` zeigt die effektiven Rechte je Akteur mit Quelle und Vollstrecker und
benennt, was auf dem System **nicht** durchsetzbar ist.

**Die Spur: `sepp audit`.** Jede Guard-Entscheidung landet als eigener Session-Eintrag — auch die
erlaubten, sonst sagt die Spur nur, was schiefging, nie was normal war. Jede `task`-Delegation
schreibt eine eigene Kind-Session, die im Header auf ihre Wurzel verweist; im Audit wird sie
eingerückt aufgeklappt. `sepp audit` gibt das Ganze lesbar aus:

```
$ sepp audit
Sitzung 9016ce87-07f5-4829-9801-2da4c5568527  ·  2026-09-05 12:53:44Z  ·  6 Einträge
Verzeichnis /home/du/projekt

12:53:44  Nutzer    Delegiere: Lies README.md und nenne den Projektnamen.
12:53:45  Tool →    task {"description":"Lies die Datei README.md und nenne den Projektnamen."}
12:53:48  Guard     ALLOW · agent · lesen README.md
12:53:48  Sub-Agent 4586db5f · „Lies die Datei README.md …" · 4 Einträge
  12:53:45  Nutzer    Lies die Datei README.md und nenne den Projektnamen.
  12:53:47  Tool →    read {"path":"README.md"}
  12:53:47  Tool ✓    # rusty — ein winziges Testprojekt
  12:53:48  Modell    Das Projekt heißt "rusty".
12:53:48  Tool ✓    Das Projekt heißt "rusty".
12:53:49  Modell    Das Projekt heißt "rusty".

1 Prompts · 1 Tool-Aufrufe · 0 verweigert · 1 Sub-Agenten
```

Ohne Argument die jüngste Sitzung des Projekts, sonst ein ID-Präfix. `--no-children` lässt die
Kind-Sessions zu; `--json` gibt ein Objekt je Eintrag aus, etwa für
`sepp audit --json | jq 'select(.entry.payload.kind == "guard")'`. Session-Dateien liegen mit
`0600` in einem `0700`-Verzeichnis — sie enthalten alles, was der Agent gelesen hat.

**Grenzen, ehrlich benannt:** Landlock kennt keine Verbote unterhalb einer Gewährung (ein Deny
unter `fs_read = ["~"]` gilt für `bash` nicht, für `read`/`write`/`edit` schon; `sepp policy`
meldet solche Überlappungen). Netz ist für Kindprozesse (`bash`, MCP-stdio) „ganz oder gar
nicht"; der Host-Filter kommt dort mit dem Egress-Proxy, und deshalb sperrt auch ein `[deny] net`
mit Hostliste alles, mit Hinweis. Für WASM-Plugins gilt der Host-Filter dagegen **exakt je
Anfrage**: Ein Plugin hat keine Sockets, nur `host_http`, und sepp ist sein Netzwerkstack —
Allowlist, Secrets (`$NAME` in Header-Werten, Doppel-Gate `net` + `env`), keine Redirects, jede
Anfrage in der Audit-Spur. Verbieten lassen sich nur Pfade und Netz; `exec` und `env` kann `[deny]` nicht, weil
Landlock für beides nur Erlaubnislisten kennt. Das TCP-Verbot braucht Landlock ABI 4 (Kernel ≥ 6.7). Exec-Listen sind
auf macOS wegen Apples Shims fragil. Unter Guard verliert die Shell alle nicht freigegebenen
Umgebungsvariablen (`[agent].env` ist der Schalter). Im Audit stehen Entscheidungen aus einem
Sub-Agent-Lauf in der Wurzel-Spur, nicht in der Kind-Session: alle Tools teilen sich einen Guard,
und eine Aufteilung könnte bei parallelen Aufrufen der falschen Sitzung zugeschlagen werden.

Schwachstellen melden: [`SECURITY.md`](./SECURITY.md).

## Architektur

Cargo-Workspace aus kleinen Crates mit strikten Schichtgrenzen (untere Crates importieren nie obere):

```
sepp-core      Typen + reine Logik (kein I/O, kein tokio)
  ├── sepp-provider   Provider-Trait + Anthropic/OpenAI (HTTP/SSE)
  ├── sepp-tools      built-in Tools read/write/edit/bash (unter Sepp Guard) + Truncation
  ├── sepp-session    Baum-Sessions (JSONL, optional SQLite)
  ├── sepp-plugin     Guest-SDK für WASM-Plugins (Ziel wasm32; + sepp-plugin-macros für #[tool])
  ├── sepp-pkg        Paketformat .seppkg: Manifest, Hash, Signatur (ring), Container (tar+zstd), Installation, Registry-Index
  └── sepp-policy     Capabilities / Policy / Sepp Guard / Sandbox (Landlock, Seatbelt) / Secret-Broker
        ├── sepp-hooks  Rhai-Hook-Bus
        ├── sepp-wasm   WASM-Plugin-Host (wasmi); Vertrag: wit/sepp.wit
        └── sepp-mcp    MCP-Client als Tool-Quelle
sepp-agent     Agent-Loop, Tool-Dispatch, Budget, Sub-Agenten (bindet alle sepp-*)
sepp-cli       Frontends: TUI / One-shot / RPC; sepp init / policy / audit / plugin new / pkg
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

## Mitwirken

Beiträge sind willkommen — siehe [`CONTRIBUTING.md`](./CONTRIBUTING.md) und den
[Code of Conduct](./CODE_OF_CONDUCT.md). Issues und PRs bitte über GitHub.

## Lizenz

Lizenziert unter der [PolyForm Noncommercial License 1.0.0](./LICENSE) — eine
**source-available**-Lizenz, die **ausschließlich nicht-kommerzielle Nutzung** erlaubt. Der
Patent-Grant gilt nur für diese erlaubte (nicht-kommerzielle) Nutzung; ein kommerzieller
Patent-Grant wird nicht gewährt. Für kommerzielle Nutzung bitte den Autor kontaktieren. Sofern
nicht anders angegeben, werden beigesteuerte Beiträge unter denselben Bedingungen aufgenommen.
