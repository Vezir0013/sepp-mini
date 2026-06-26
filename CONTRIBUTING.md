# Mitwirken an sepp mini

Danke, dass du beitragen möchtest! Dieses Dokument fasst zusammen, wie du das Projekt baust,
testest und Änderungen einreichst.

## Voraussetzungen

- Rust **stable** (die Toolchain ist über `rust-toolchain.toml` gepinnt — `rustup` wählt sie
  automatisch). Komponenten: `rustfmt`, `clippy`.
- Optional: [`just`](https://github.com/casey/just) als Task-Runner.
- Reine Code-Arbeit braucht **keinen** API-Key — Live-LLM-Tests sind per Default geskippt.

## Bauen & Testen

`just check` ist **das Tor**: Eine Änderung gilt erst als fertig, wenn es grün ist.

```bash
just check    # cargo fmt --check && clippy --workspace -D warnings && cargo test
just build    # cargo build --workspace
just test     # Tests (cargo nextest run, Fallback cargo test)
just audit    # cargo audit && cargo deny check
```

Ohne `just` die darunterliegenden `cargo`-Kommandos direkt nutzen. Einen einzelnen Test laufen
lassen: `cargo test <name>` bzw. `cargo nextest run <name>`.

Optionale Features mit-testen (SQLite, OpenAI):

```bash
cargo test --workspace --all-features
```

Provider-Parser werden gegen aufgezeichnete SSE-Fixtures getestet, nicht gegen das echte Netz.
Sandbox-Negativtests (Landlock) sind `#[ignore]`-gated und laufen via `cargo test -- --ignored`
auf einem Linux-Host mit durchsetzbarem Landlock.

## Konventionen

- **Kleine, grüne Schritte.** Eine logische Einheit pro Commit.
- **Conventional Commits**: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:` …
- **Keine `unwrap()`/`expect()`/`panic!`/`todo!`/`unimplemented!`** in Library-Crates
  (`crates/*` außer `sepp-cli`/Tests). Stattdessen `Result<T, SeppError>` mit Kontext.
- **Tool-Output ist immer getrunkt**, bevor er ins LLM geht.
- **Logs gehen nach STDERR** — stdout ist der Datenkanal (RPC/MCP). Secrets nie loggen.
- **Dependencies exakt pinnen** (`=x.y.z`); `Cargo.lock` ist committet.
- Öffentliche Items mit `///` dokumentieren.

## Pull Requests

1. Branch von `main` erstellen.
2. Änderung mit Tests implementieren; `just check` grün halten.
3. Öffentliche API dokumentieren; bei Format-/Protokoll-Änderungen nur additiv arbeiten.
4. PR mit klarer Beschreibung öffnen (was/warum). Das CI-Tor (fmt + clippy + test + audit) muss
   grün sein.

## Sicherheit

Schwachstellen **nicht** über öffentliche Issues melden — siehe [`SECURITY.md`](./SECURITY.md).

## Lizenz

Mit dem Einreichen eines Beitrags stimmst du zu, dass dieser unter der
[PolyForm Noncommercial License 1.0.0](./LICENSE) lizenziert wird.
