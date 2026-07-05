# Changelog

Alle nennenswerten Änderungen an diesem Projekt werden hier dokumentiert.

Das Format orientiert sich an [Keep a Changelog](https://keepachangelog.com/de/1.1.0/),
und das Projekt folgt [Semantic Versioning](https://semver.org/lang/de/).

## [Unreleased]

### Geplant
- OpenTelemetry-Export (optional aktivierbar)
- OAuth-Login für Subscription-Provider
- Google-Provider-Adapter
- Netz-Sandbox für MCP-Subprozesse (seccomp/Namespaces)

## [0.1.12] - 2026-07-05

Review-Härtung des 0.1.11-Umfangs (`--provider mlx`, TUI `/hide`/`/show`) — Fokus Sicherheit,
Performance und Plattform-Robustheit.

### Behoben
- **OpenAI-Streaming-Mapper trennt Tool-Calls bei Index-Recycling korrekt.** Server der
  llama.cpp-Familie (LM Studios Engine) streamen teils jeden Tool-Call erneut unter `index:0`
  mit neuer id; bisher wurden die Argumente beider Calls unter der ersten id konkateniert
  (ungültiges JSON, zweites Tool lief nie). Neues SSE-Fixture `openai_repeated_index.sse`
  deckt den Fall ab; leere Tool-Call-ids zählen jetzt wie „keine id".
- **TUI: `/show` friert die UI nicht mehr ein.** Der Handler lockte den Session-Mutex, den ein
  laufender Prompt-Task für die gesamte Turn-Dauer hält — die Event-Loop stand bis Turn-Ende
  (kein Rendern, kein Esc-Abbruch). `/hide`/`/show` togglen jetzt lock-frei.
- **TUI: getippte Eingabe geht bei laufendem Turn nicht mehr verloren** — der Text bleibt in
  der Eingabezeile stehen statt kommentarlos verworfen zu werden.
- **Leere `OPENAI_BASE_URL` (`=""`) zählt jetzt wie „nicht gesetzt"** — vorher ging der
  Preset-Default verloren (roher „relative URL"-Fehler beim ersten Request) und der
  mlx-Erreichbarkeits-Check wurde übersprungen.
- **mlx-Fehler melden sich als „mlx", nicht als „openai"** (`name()` + alle Fehlertexte) —
  LM-Studio-Probleme werden nicht mehr dem falschen Anbieter zugeschrieben.
- **Verbindungsfehler nennen den Endpunkt** („Verbindung zu … fehlgeschlagen — läuft der
  Server?") statt eines rohen reqwest-Texts — deckt auch den Fall ab, dass der Server nach dem
  Preflight stirbt.
- **TUI: Cursor-Überlauf behoben** — die Cursor-Spalte wird in `usize` gerechnet (kein
  Debug-Panic mehr bei sehr langen Eingaben/Paste), erst final geclampt.
- **Doku-Drift repariert:** `sepp --help` listet `/hide` `/show`; `--provider`-Aufzählungen
  (RunOpts-Doku, README) nennen `zai` und `mlx`; CHANGELOG-Linkblock 0.1.5–0.1.12 nachgezogen.

### Geändert
- **mlx-Preflight nicht-blockierend und IPv4+IPv6-korrekt:** async `tokio::net`-Connect mit
  700-ms-Timeout gegen den Hostnamen `localhost:1234` (getaddrinfo probiert `::1` UND
  `127.0.0.1`) statt eines synchronen IPv4-only-Syscalls auf dem Runtime-Thread; der
  Default-Endpunkt lebt jetzt als eine `pub`-Konstante in `sepp-provider` (Konsistenz per
  Unit-Test gesichert), Meldungstexte leiten sich daraus ab.
- **Hinweis, wenn `--think`/`SEPP_THINK` wirkungslos ist** (`--provider openai|local|mlx`
  senden kein Reasoning-Feld) — vorher ein stiller No-op.
- **TUI: Meldungen erreichen bei versteckter Statuszeile den Chatverlauf** (`notify`-Helfer;
  Fehler rot) — Feedback wie „läuft noch", `/model`-Ausgaben oder Befehls-Fehler verpufft
  nach `/hide` nicht mehr unsichtbar.
- **TUI: Warnung bei Prompt-Templates, die Builtin-Befehle verschatten** (beim Start und bei
  `/reload`) — solche Templates sind per Slash unerreichbar, der Builtin gewinnt.
- **TUI: `/model` mit unregistrierter ID erbt den Session-Provider** (korrekte
  Compaction-Schwelle, z. B. 128k statt fälschlich anthropic/200k) und zeigt die Modell-ID
  statt „(custom)"; die TUI-eigene `custom_model`-Kopie ist konsolidiert.
- **Provider-Tests mutieren die Prozess-Umgebung nicht mehr** (`remove_var` entfernt;
  base_url-Auflösung als pure, direkt getestete Funktion) — kein Data-Race-Fenster mehr im
  parallelen Test-Binary.

### Sicherheit
- **`--provider mlx` sendet `OPENAI_API_KEY` nur noch bei explizit gesetztem
  `OPENAI_BASE_URL`.** Im Zero-Config-Fall geht kein Bearer-Token mehr über Klartext-HTTP an
  den lokalen Port 1234 — ein für andere Tools exportierter echter OpenAI-Key kann nicht mehr
  an einen fremden lokalen Prozess oder in Server-Logs lecken. Wer LM Studio mit aktivierter
  Auth nutzt, setzt `OPENAI_BASE_URL=http://localhost:1234/v1` als bewusstes Opt-in.

## [0.1.11] - 2026-07-01

### Hinzugefügt
- **`--provider mlx` — Zero-Config-Verbindung zu lokaler MLX-Inferenz via LM Studio.** Der lokale
  OpenAI-kompatible Server von LM Studio wird ohne Konfiguration erreicht: `--provider mlx` zielt
  standardmäßig auf `http://localhost:1234/v1` (statt api.openai.com), API-Key optional. Das Modell
  wählt der Nutzer mit `-m` (passend zum in LM Studio geladenen Modell) — sepp gibt kein Modell vor.
  Ist der Server nicht erreichbar oder fehlt `-m`, bricht sepp früh mit einer hilfreichen Meldung ab
  statt mit einem rohen Connection-Fehler. `OPENAI_BASE_URL` überschreibt den Endpunkt (abweichender
  Host/Port).
- **TUI: `/hide` und `/show`** blenden die gelbe Statuszeile aus/ein — mehr Platz im Terminal; die
  Statuszeile wird nur eingeplant (gerendert), wenn sie sichtbar ist.

## [0.1.10] - 2026-07-01

### Hinzugefügt
- **macOS: OS-Dateisystem-Sandbox für MCP-Subprozesse via Seatbelt** (`sandbox_init`, rohes
  SBPL-Profil im `pre_exec` des Kindes). Damit erhalten stdio-MCP-Server auf macOS dieselbe
  Absicherung wie unter Linux-Landlock — Scope Dateisystem + Environment-Scrubbing, **fail-closed**
  (schlägt `sandbox_init` fehl, wird der Subprozess nicht ungesandboxt gestartet). Read- und
  Write-Confinement auf echtem macOS (26.x) verifiziert. Nur Plattformen ohne Adapter (Windows/BSD)
  fallen weiterhin auf `NullSandbox` mit Warnung zurück.

### Geändert
- **`install.sh` trägt den PATH automatisch ein.** Liegt das Zielverzeichnis (Default
  `~/.local/bin`) nicht im PATH, ergänzt der Installer idempotent eine PATH-Zeile in der zur
  Login-Shell passenden Profildatei (`~/.zprofile` / `~/.bash_profile` / `~/.profile`). Damit ist
  der macOS-Install 1:1 wie unter Linux — kein manueller PATH-Schritt mehr. System-Installationen
  (`/usr/local/bin`) sind ohnehin im PATH und bleiben unberührt.

### Entfernt
- **Token-Verbrauch-Anzeige komplett entfernt.** Die Mini-Tabelle am Ende der Konversation
  (One-shot/TUI), die maschinenlesbare `usage_summary`-Zeile im RPC-Modus und der persistierte
  `usage_summary`-Eintrag in der Session-Datei entfallen samt der internen kumulativen
  Token-Buchhaltung (`total_usage`/`turns`). Die per-Turn-`usage` an jeder Assistant-Nachricht
  bleibt erhalten (Provider-Daten); `last_usage` bleibt als Basis der Auto-Compaction-Schwelle und
  `model_label` weiterhin für die TUI-Statuszeile. Alte Sessions mit `usage_summary`-Einträgen
  bleiben les- und ladbar (generischer Custom-Eintrag).

## [0.1.9] - 2026-06-29

### Geändert
- **FHS-Layout: die globale Wurzel ist in `config_root` und `state_root` getrennt.** config_root
  (`settings.toml`, `skills/`, `prompts/`, `hooks/`, `plugins/`): `$SEPP_CONFIG_DIR` → `$SEPP_HOME`
  → vorhandenes `~/.sepp` → vorhandenes `/etc/sepp` → `~/.sepp`. state_root (`sessions/`,
  `trust.json`): analog mit `$SEPP_STATE_DIR` und `/var/lib/sepp`. **Default bleibt die eine Wurzel
  `~/.sepp`**; der Split greift nur, wenn die Env-Variablen gesetzt sind oder ein System-Setup
  existiert. `SEPP_HOME` setzt weiterhin beide Wurzeln (rückwärtskompatibel).
- **Sessions liegen wieder zentral** unter `state_root/sessions/<hash(cwd)>/` (kehrt die
  projektlokale Ablage aus 0.1.8 um). Projektlokales `<repo>/.sepp` enthält jetzt **nur Config**
  (skills/prompts/hooks/plugins/settings.toml); `sepp init` legt dort kein `sessions/` und keine
  `.gitignore` mehr an.

### Hinzugefügt
- **`sepp init --system`**: legt das FHS-Layout in einem Befehl an (`/etc/sepp` config +
  `/var/lib/sepp` state, state_root `0700`) und nennt die passenden Env-Exports. Über
  `$SEPP_CONFIG_DIR`/`$SEPP_STATE_DIR` umlenkbar.
- **`install.sh --system`**: installiert die Binary nach `/usr/local/bin` und ruft `sepp init
  --system` — Systeminstallation in einem Schritt.
- **`sepp uninstall --purge` räumt beide Wurzeln** (config_root + state_root) plus projektlokale
  `.sepp` via Trust-Registry. `install.sh --uninstall` delegiert nun an die Binary (behebt, dass es
  vorher `~/.sepp` hartkodierte und `SEPP_HOME` ignorierte).

## [0.1.8] - 2026-06-29

### Geändert
- **`sepp uninstall --purge` entfernt jetzt auch projektlokale `.sepp`-Verzeichnisse.** Neben dem
  globalen Root (`~/.sepp`/`$SEPP_HOME`, enthält Keys/Trust) werden alle projektlokalen `.sepp`
  entfernt, die `sepp init` in der Trust-Registry (`trust.json`) vermerkt hat — standortunabhängig
  (z. B. `/home/.sepp`, egal aus welchem Verzeichnis `uninstall` läuft). Vorher traf `--purge` nur
  den globalen Root, sodass projektlokale Installationen verwaist zurückblieben. Jede Aktion wird
  einzeln gemeldet; entfernt werden ausschließlich `…/.sepp`-Unterordner, nie die Projektordner.
- **Sessions liegen jetzt projektlokal** unter `<repo>/.sepp/sessions/<uuid>.jsonl` (vorher global
  `~/.sepp/sessions/<hash(cwd)>/`). Dadurch reisen Session-Logs mit dem Projekt. **`SEPP_HOME`
  verschiebt Sessions nicht mehr** (steuert weiterhin globale Config/Resources/Trust). Alte globale
  Sessions werden von `-c`/`-r` nicht mehr gefunden (keine Migration — Logs sind ephemer).
- **Token-Live-Anzeige in der TUI-Statuszeile entfernt** — sie zeigt nur noch das Modell. Der
  detaillierte Token-Verbrauch erscheint stattdessen als Mini-Tabelle am Ende der Konversation.

### Hinzugefügt
- **`sepp init` legt `sessions/` und eine `.gitignore` mit an** (idempotent). Die `.gitignore`
  schützt projektlokale Laufzeitdaten (Session-Logs, `trust.json`, SQLite) vor versehentlichem
  Commit; das Config-Skelett bleibt teilbar.
- **Audit jeder Start**: Der Session-Store wird vor der API-Key-Prüfung gebaut. Bricht der Start ab
  (z. B. fehlender Key), wird ein `aborted`-Eintrag geschrieben und fsync't — die Session-Datei
  existiert also auch bei fehlgeschlagenem Start. Provider-Fehler mitten in der Konversation flushen
  jetzt ebenfalls (Audit-Trail durabel).
- **Session-weite Token-Buchhaltung**: kumulative Summe (Input/Output/Cache) über alle Turns, am
  Ende der Konversation als `usage_summary`-Eintrag in der Session-Datei persistiert und als
  Mini-Tabelle angezeigt (One-shot/RPC → stderr, TUI → beim Quit). RPC emittiert beim Shutdown eine
  maschinenlesbare `usage_summary`-Zeile.

## [0.1.7] - 2026-06-29

### Geändert
- **`sepp init` legt die Konfig jetzt projektlokal an** (`<cwd>/.sepp`) statt global in `~/.sepp`
  und vertraut das Verzeichnis automatisch, damit es sofort geladen wird. Für die globale Wurzel:
  `sepp init --global`. **Achtung: Default-Verhalten geändert** — wer das alte Verhalten will,
  nutzt `--global`.

### Hinzugefügt
- **`SEPP_HOME`** verlegt die globale Konfig-Wurzel konsistent für Anlegen, Laden und Trust
  (Default `~/.sepp`, Konvention wie `CARGO_HOME` — der Wert ist direkt die Wurzel). Behebt, dass
  die Konfig als root unter `/root/.sepp` landete.

## [0.1.6] - 2026-06-29

### Geändert
- **z.ai ist jetzt ein eigenständiger Connector** (`ZaiProvider`, Modul `sepp-provider::zai`,
  Feature `zai = ["openai"]`) statt eines Dialekt-Flags auf dem OpenAI-Adapter. `name()` liefert
  `"zai"`, und alle Fehler-/Stream-Texte tragen `zai:` statt `openai:` — ein z.ai-Fehler erschien
  vorher fälschlich als OpenAI-Fehler. Das OpenAI-kompatible Drahtformat (SSE-Decoder,
  Request-Builder) wird weiterhin geteilt; dupliziert wird nichts.

### Behoben
- **Falsches Endpunkt-Routing bei GLM-Modellen.** Ohne `--provider`/`SEPP_PROVIDER` wird der
  Provider nun aus dem Modell abgeleitet (`-m glm-5.2` → `zai`). Bisher konnte ein GLM-Modell an
  `api.openai.com` gesendet werden und scheiterte dort am 401 („You didn't provide an API key").
  Die Mismatch-Warnung greift jetzt auch für GLM-Modelle auf `--provider local/openai` (vorher
  stillschweigend unterdrückt).
- **Sicherheits-Advisory `anyhow`** auf `1.0.103` angehoben (RUSTSEC-2026-0190: Unsoundness in
  `Error::downcast_mut()`). `cargo deny check` ist damit wieder grün und der Release-Build läuft.

### Tests
- **z.ai Live-Smoke-Test** (`crates/sepp-provider/tests/zai_live.rs`). Per Default `#[ignore]`;
  läuft nur über `just test-live` mit gesetztem `ZAI_API_KEY` und macht einen minimalen echten
  Call gegen api.z.ai (kein `Error`-Event, sauberer MessageStart…MessageStop, etwas Text). Ohne
  Schalter/Key ein stiller No-op.

## [0.1.5] - 2026-06-29

### Hinzugefügt
- **z.ai / Zhipu-GLM als Provider** (`--provider zai` bzw. `SEPP_PROVIDER=zai`). Nutzt den
  OpenAI-kompatiblen Endpunkt `https://api.z.ai/api/paas/v4` über den bestehenden OpenAI-Adapter —
  kein neuer Parser. Key aus `ZAI_API_KEY` (Format `id.secret`), Endpunkt über `ZAI_BASE_URL`
  überschreibbar (z. B. China-Region). GLM-5.2/4.6/4.5-Air/4.5-Flash sind in der Modell-Registry
  hinterlegt (Default-Modell `glm-5.2`, das aktuelle Flaggschiff); Kontextfenster/Limits sind
  konservativ und gegen die z.ai-Docs zu verifizieren. Fehlt der Key, scheitert der Start mit
  einem hilfreichen Hinweis.
- **OpenAI-Adapter: `reasoning_content` → ThinkingDelta.** Reasoning-Modelle über
  OpenAI-kompatible Endpunkte (z. B. GLM-5.2/4.6, DeepSeek-R1) streamen ihr Denken im Feld
  `reasoning_content`; das wird jetzt als Thinking abgebildet statt verworfen (No-op für reine
  Chat-Modelle).
- **Reasoning-Steuerung.** `--think`/`--no-think` und `SEPP_THINK` (gelayert wie `SEPP_PROVIDER`,
  Flag gewinnt) schalten das Denken ein/aus; bei `--provider zai` (GLM) ist Reasoning **per Default
  an**, andere Provider bleiben unverändert. Der z.ai-Adapter sendet dafür `thinking:{type:…}`
  (binär, nur am z.ai-Endpunkt; explizit `disabled` spart bei Trivialfragen ~Faktor 77
  completion_tokens). Anzeige gedimmt sichtbar (Opt-out `--hide-thinking`): One-shot streamt das
  Denken nach **STDERR** (stdout bleibt reiner Datenkanal), die TUI zeigt es gedimmt im Verlauf,
  RPC liefert weiterhin `{"type":"thinking"}`. Hinweis: das Denken (Chain-of-Thought) wird wie die
  Antwort in der Session-JSONL persistiert; an die Provider zurückgespielt wird es nicht.

## [0.1.4] - 2026-06-28

### Hinzugefügt
- **`sepp uninstall`** entfernt die installierte Binary direkt aus sich selbst; mit `--purge`
  zusätzlich `~/.sepp` (Sessions + Config). Ohne `--purge` bleiben die Nutzerdaten bewusst stehen.
- **`install.sh --uninstall`** (optional `--purge`) als Shell-Weg für denselben Zweck — nützlich,
  wenn die Binary bereits entfernt wurde. Der Installer parst Argumente jetzt über eine echte
  Schleife (Kombinationen wie `--uninstall --purge` in beliebiger Reihenfolge); unbekannte Flags
  werden nun als Fehler gemeldet statt ignoriert.
- **`sepp init`** legt das Konfigurations-Skelett `~/.sepp/{skills,prompts,hooks,plugins}/` samt
  kommentierter Beispiel-`settings.toml` an. Idempotent — vorhandene Dateien bleiben unberührt.
- **Erst-Start-Hinweis:** Fehlt bei Default-Provider Anthropic der `ANTHROPIC_API_KEY`, erklärt eine
  mehrzeilige Meldung jetzt die Optionen (Key setzen · `--provider local`/`OPENAI_BASE_URL` · OpenAI)
  und verweist auf `~/.sepp` bzw. `sepp init`.

## [0.1.3] - 2026-06-26

### Geändert
- **Lizenz von Apache-2.0 auf PolyForm Noncommercial 1.0.0 umgestellt.** `sepp mini` ist damit
  *source-available* und darf **ausschließlich für nicht-kommerzielle Zwecke** genutzt werden.
  Der Patent-Grant gilt nur für diese erlaubte Nutzung; ein kommerzieller Patent-Grant wird nicht
  gewährt. Betrifft `LICENSE`, `NOTICE`, die `Cargo.toml`-Metadaten, `README.md` und
  `CONTRIBUTING.md`. Für kommerzielle Nutzung bitte den Autor kontaktieren.
- `cargo-deny`-Allowlist um `PolyForm-Noncommercial-1.0.0` ergänzt (für die eigenen
  Workspace-Crates), damit das Supply-Chain-Gate grün bleibt. Die Allowlist für
  Abhängigkeits-Lizenzen (u. a. `Apache-2.0`) bleibt unverändert.

> Hinweis: Der frühere Release `v0.1.0` bleibt unter Apache-2.0 lizenziert. Die Umstellung gilt
> ab `v0.1.3`.

## [0.1.0] - 2026-06-24

Erste öffentliche Version. Funktional vollständig und getestet.

### Hinzugefügt
- **Agent-Kern** (`sepp-core`, `sepp-provider`, `sepp-tools`, `sepp-agent`): Streaming-Loop mit
  parallelem Tool-Dispatch (tokio `JoinSet`), Cancellation, Kontext-Budget und Auto-Compaction.
  Eingebaute Tools `read`/`write`/`edit`/`bash` mit verpflichtender Output-Trunkierung und
  pro-Pfad serialisierten Datei-Mutationen.
- **Anthropic-Provider** (Messages API) mit handgeschriebenem SSE-Decoder (gegen Fixtures getestet).
- **Interaktive TUI** (ratatui/crossterm) mit Slash-Commands (`/new` `/resume` `/tree` `/compact`
  `/model` `/trust` `/reload` …) sowie **One-shot** (`-p`).
- **Persistente Baum-Sessions** als JSONL (Default) mit Branching und Compaction; optional
  **SQLite**-Backend (`--features sqlite`, WAL).
- **Erweiterbarkeit (4 Tiers):** Resources (Skills→System-Prompt, Prompt-Templates→Slash-Commands),
  Hooks (Rhai), WASM-Plugins (capability-gated, via `wasmi`), MCP-Server (rmcp-Client als Tool-Quelle).
- **Sicherheitsmodell:** `sepp-policy` mit `Capability`/`Policy`, Manifest-Parser, OS-Sandbox via
  **Landlock** (fail-closed, wenn nicht durchsetzbar) und Environment-Scrubbing für Subprozesse;
  Secret-Broker; projektlokale Erweiterungen erst nach Trust.
- **Native Sub-Agenten:** isolierter Kontext, eingeschränktes Toolset, eigenes Budget.
- **Multi-Provider:** OpenAI-kompatibler Adapter (inkl. lokaler Endpunkte via `OPENAI_BASE_URL`),
  Auswahl per `--provider` / `SEPP_PROVIDER`.
- **JSONL-RPC-Modus** (`--rpc`) zum Einbetten in andere Programme — selber Kern wie TUI/One-shot.
- **Distribution:** statische Binaries (CI-Matrix Linux musl + macOS), `install.sh`,
  `cargo audit` + `cargo deny` in CI.

### Sicherheit
- Subprozesse (MCP, `bash`) erben keine API-Keys mehr (Environment-Scrubbing bzw. gezieltes
  Entfernen von Provider-Keys).
- Landlock verfährt fail-closed, wenn der Kernel die Durchsetzung nicht garantiert.
- MCP- und WASM-Tool-Ausgaben werden vor dem Kontextfenster getrunkt; WASM-Rückgaben und der
  SSE-Decoder sind gegen unbegrenztes Speicherwachstum abgesichert.

[Unreleased]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.12...HEAD
[0.1.12]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.11...v0.1.12
[0.1.11]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.10...v0.1.11
[0.1.10]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.9...v0.1.10
[0.1.9]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.0...v0.1.3
[0.1.0]: https://github.com/Vezir0013/sepp-mini/releases/tag/v0.1.0
