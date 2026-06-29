# Changelog

Alle nennenswerten Änderungen an diesem Projekt werden hier dokumentiert.

Das Format orientiert sich an [Keep a Changelog](https://keepachangelog.com/de/1.1.0/),
und das Projekt folgt [Semantic Versioning](https://semver.org/lang/de/).

## [Unreleased]

### Hinzugefügt
- **z.ai / Zhipu-GLM als Provider** (`--provider zai` bzw. `SEPP_PROVIDER=zai`). Nutzt den
  OpenAI-kompatiblen Endpunkt `https://api.z.ai/api/paas/v4` über den bestehenden OpenAI-Adapter —
  kein neuer Parser. Key aus `ZAI_API_KEY` (Format `id.secret`), Endpunkt über `ZAI_BASE_URL`
  überschreibbar (z. B. China-Region). GLM-4.6/4.5-Air/4.5-Flash sind in der Modell-Registry
  hinterlegt (Default-Modell `glm-4.6`); Kontextfenster/Limits sind konservativ und gegen die
  z.ai-Docs zu verifizieren. Fehlt der Key, scheitert der Start mit einem hilfreichen Hinweis.
- **OpenAI-Adapter: `reasoning_content` → ThinkingDelta.** Reasoning-Modelle über
  OpenAI-kompatible Endpunkte (z. B. GLM-4.6, DeepSeek-R1) streamen ihr Denken im Feld
  `reasoning_content`; das wird jetzt als Thinking abgebildet statt verworfen (No-op für reine
  Chat-Modelle).

### Geplant
- OpenTelemetry-Export (optional aktivierbar)
- OAuth-Login für Subscription-Provider
- Google-Provider-Adapter
- Netz-Sandbox für MCP-Subprozesse (seccomp/Namespaces)

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

[Unreleased]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.0...v0.1.3
[0.1.0]: https://github.com/Vezir0013/sepp-mini/releases/tag/v0.1.0
