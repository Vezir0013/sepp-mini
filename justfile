# Task-Runner für sepp mini (https://github.com/casey/just)
# Ohne `just` die jeweils darunterliegenden cargo-Kommandos direkt nutzen.

# Standard: verfügbare Targets anzeigen
default:
    @just --list

# Workspace bauen
build:
    cargo build --workspace

# DAS Tor: Format-Check + Clippy (Warnungen = Fehler) + Tests
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# Nur Tests (nextest, falls installiert; sonst cargo test)
test:
    cargo nextest run --workspace || cargo test --workspace

# Live-Tests inkl. LLM (benötigt API-Key in der Umgebung)
test-live:
    SEPP_LIVE_TESTS=1 cargo test --workspace -- --include-ignored

# Formatieren
fmt:
    cargo fmt --all

# Clippy mit Autofix (vorsichtig verwenden)
fix:
    cargo clippy --workspace --all-targets --fix --allow-dirty -- -D warnings

# CLI ausführen, z. B.: just run -- -p "hallo"
run *ARGS:
    cargo run -p sepp-cli -- {{ARGS}}

# Supply-Chain-Gate: bekannte Vulns + Lizenzen/Bans/Quellen
audit:
    cargo audit
    cargo deny check

# Release-Binary bauen (inkl. SQLite-Backend)
release:
    cargo build --release -p sepp-cli --features sqlite

# Statische Linux-Binary (musl) für die Distribution
release-static:
    rustup target add x86_64-unknown-linux-musl
    cargo build --release -p sepp-cli --features sqlite --target x86_64-unknown-linux-musl

# Aufräumen
clean:
    cargo clean

# Beispiel-Plugin (Tier 2, WASM) bauen — siehe examples/textstat-plugin/README.md
plugin-example:
    rustup target add wasm32-unknown-unknown
    cargo build --release --target wasm32-unknown-unknown \
        --manifest-path examples/textstat-plugin/Cargo.toml
    @echo ""
    @echo "Fertig. Zum Installieren beide Dateien nach ~/.sepp/plugins/ kopieren:"
    @echo "  cp examples/textstat-plugin/target/wasm32-unknown-unknown/release/textstat.wasm ~/.sepp/plugins/"
    @echo "  cp examples/textstat-plugin/textstat.toml ~/.sepp/plugins/"
