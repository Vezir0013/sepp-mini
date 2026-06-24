#!/bin/sh
# sepp mini — Installer.
#
# Lädt eine vorgebaute statische Binary aus den GitHub-Releases oder baut aus dem Quellcode.
#
# Nutzung:
#   curl -fsSL <raw-url>/install.sh | sh
#   SEPP_REPO=owner/repo SEPP_BIN_DIR=~/.local/bin sh install.sh
#   sh install.sh --from-source        # via `cargo install` (braucht Rust-Toolchain)
#
# Umgebung:
#   SEPP_REPO     GitHub "owner/repo" (Default: aus dem Repo-Override unten)
#   SEPP_VERSION  Tag, z. B. v0.1.0 (Default: latest)
#   SEPP_BIN_DIR  Zielverzeichnis (Default: ~/.local/bin)
set -eu

REPO="${SEPP_REPO:-Vezir0013/sepp-mini}"
VERSION="${SEPP_VERSION:-latest}"
BIN_DIR="${SEPP_BIN_DIR:-$HOME/.local/bin}"

log() { printf '%s\n' "$*" >&2; }
die() { log "Fehler: $*"; exit 1; }

from_source() {
    command -v cargo >/dev/null 2>&1 || die "cargo nicht gefunden (Rust-Toolchain nötig für --from-source)"
    log "Baue aus dem Quellcode via cargo install (Feature sqlite)…"
    cargo install --git "https://github.com/${REPO}" --features sqlite sepp-cli
    log "Installiert nach \$CARGO_HOME/bin (oder ~/.cargo/bin)."
    exit 0
}

[ "${1:-}" = "--from-source" ] && from_source

# Plattform erkennen.
os="$(uname -s)"; arch="$(uname -m)"
case "$os" in
    Linux)  target_os="unknown-linux-musl" ;;
    Darwin) target_os="apple-darwin" ;;
    *) die "nicht unterstütztes OS: $os (versuche: sh install.sh --from-source)" ;;
esac
case "$arch" in
    x86_64|amd64)  target_arch="x86_64" ;;
    arm64|aarch64) target_arch="aarch64" ;;
    *) die "nicht unterstützte Architektur: $arch" ;;
esac
asset="sepp-${target_arch}-${target_os}"

case "$REPO" in
    OWNER/*) die "Bitte SEPP_REPO=owner/repo setzen (oder: sh install.sh --from-source)" ;;
esac

if [ "$VERSION" = "latest" ]; then
    url="https://github.com/${REPO}/releases/latest/download/${asset}"
else
    url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
fi

command -v curl >/dev/null 2>&1 || die "curl nicht gefunden"
mkdir -p "$BIN_DIR"
log "Lade ${url} …"
curl -fsSL "$url" -o "${BIN_DIR}/sepp" || die "Download fehlgeschlagen ($url)"
chmod +x "${BIN_DIR}/sepp"
log "Installiert: ${BIN_DIR}/sepp"
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) log "Hinweis: ${BIN_DIR} ist nicht im PATH — ergänze es in deiner Shell-Konfig." ;;
esac
"${BIN_DIR}/sepp" --version || true
