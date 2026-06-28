#!/bin/sh
# sepp mini — Installer.
#
# Lädt eine vorgebaute statische Binary aus den GitHub-Releases oder baut aus dem Quellcode.
#
# Nutzung:
#   curl -fsSL <raw-url>/install.sh | sh
#   SEPP_REPO=owner/repo SEPP_BIN_DIR=~/.local/bin sh install.sh
#   sh install.sh --from-source        # via `cargo install` (braucht Rust-Toolchain)
#   sh install.sh --uninstall          # Binary entfernen (~/.sepp bleibt)
#   sh install.sh --uninstall --purge  # zusätzlich ~/.sepp (Sessions + Config) löschen
#   curl -fsSL <raw-url>/install.sh | sh -s -- --uninstall --purge   # via Pipe
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

usage() {
    log "sepp mini Installer — Optionen:"
    log "  (ohne)            Vorgebaute Binary nach \$SEPP_BIN_DIR (Default ~/.local/bin) installieren"
    log "  --from-source     Via 'cargo install' aus dem Quellcode bauen (braucht Rust)"
    log "  --uninstall       Binary entfernen (~/.sepp bleibt erhalten)"
    log "  --uninstall --purge   zusätzlich ~/.sepp (Sessions + Config) löschen"
    log "  -h, --help        Diese Hilfe"
}

from_source() {
    command -v cargo >/dev/null 2>&1 || die "cargo nicht gefunden (Rust-Toolchain nötig für --from-source)"
    log "Baue aus dem Quellcode via cargo install (Feature sqlite)…"
    cargo install --git "https://github.com/${REPO}" --features sqlite sepp-cli
    log "Installiert nach \$CARGO_HOME/bin (oder ~/.cargo/bin)."
    exit 0
}

uninstall() {
    target="${BIN_DIR}/sepp"
    if [ -e "$target" ]; then
        rm -f "$target"
        log "Entfernt: $target"
    else
        log "Nicht gefunden (übersprungen): $target"
    fi

    config_dir="$HOME/.sepp"
    if [ "$do_purge" = 1 ]; then
        if [ -d "$config_dir" ]; then
            rm -rf "$config_dir"
            log "Entfernt (--purge): $config_dir"
        else
            log "Nicht gefunden (übersprungen): $config_dir"
        fi
    elif [ -d "$config_dir" ]; then
        log "Hinweis: Nutzerdaten unter $config_dir bleiben erhalten."
        log "         Zum vollständigen Entfernen erneut mit --purge ausführen."
    fi
    log "Deinstallation abgeschlossen."
}

# Argumente einsammeln (echte Schleife, damit --uninstall --purge in beliebiger Reihenfolge geht).
mode=install
do_purge=0
do_from_source=0
while [ $# -gt 0 ]; do
    case "$1" in
        --from-source) do_from_source=1 ;;
        --uninstall)   mode=uninstall ;;
        --purge)       do_purge=1 ;;
        -h|--help)     usage; exit 0 ;;
        *) die "unbekannte Option: $1 (erlaubt: --from-source, --uninstall, --purge, --help)" ;;
    esac
    shift
done

[ "$do_purge" = 1 ] && [ "$mode" != uninstall ] && die "--purge ist nur zusammen mit --uninstall gültig"

if [ "$mode" = uninstall ]; then
    uninstall
    exit 0
fi
[ "$do_from_source" = 1 ] && from_source

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
