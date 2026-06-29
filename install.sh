#!/bin/sh
# sepp mini — Installer.
#
# Lädt eine vorgebaute statische Binary aus den GitHub-Releases oder baut aus dem Quellcode.
#
# Nutzung:
#   curl -fsSL <raw-url>/install.sh | sh
#   SEPP_REPO=owner/repo SEPP_BIN_DIR=~/.local/bin sh install.sh
#   sh install.sh --from-source        # via `cargo install` (braucht Rust-Toolchain)
#   sh install.sh --system             # systemweit: Binary nach /usr/local/bin + `sepp init --system`
#                                       # (FHS: /etc/sepp config + /var/lib/sepp state)
#   sh install.sh --uninstall          # Binary entfernen (Nutzerdaten bleiben)
#   sh install.sh --uninstall --purge  # zusätzlich config+state-Root + projektlokale .sepp löschen
#   curl -fsSL <raw-url>/install.sh | sh -s -- --uninstall --purge   # via Pipe
#
# Umgebung:
#   SEPP_REPO        GitHub "owner/repo" (Default: aus dem Repo-Override unten)
#   SEPP_VERSION     Tag, z. B. v0.1.0 (Default: latest)
#   SEPP_BIN_DIR     Zielverzeichnis der Binary (Default: ~/.local/bin; mit --system /usr/local/bin)
#   SEPP_CONFIG_DIR  Config-Wurzel (FHS-Default /etc/sepp; sonst ~/.sepp)
#   SEPP_STATE_DIR   State-Wurzel  (FHS-Default /var/lib/sepp; sonst ~/.sepp)
set -eu

REPO="${SEPP_REPO:-Vezir0013/sepp-mini}"
VERSION="${SEPP_VERSION:-latest}"

log() { printf '%s\n' "$*" >&2; }
die() { log "Fehler: $*"; exit 1; }

usage() {
    log "sepp mini Installer — Optionen:"
    log "  (ohne)            Vorgebaute Binary nach \$SEPP_BIN_DIR (Default ~/.local/bin) installieren"
    log "  --system          Systemweit: Binary nach /usr/local/bin + 'sepp init --system' (FHS-Layout)"
    log "  --from-source     Via 'cargo install' aus dem Quellcode bauen (braucht Rust)"
    log "  --uninstall       Binary entfernen (Nutzerdaten bleiben erhalten)"
    log "  --uninstall --purge   zusätzlich config+state-Root + projektlokale .sepp löschen"
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
    # Ist die Binary noch da, übernimmt sie das vollständige Entfernen selbst — beide Wurzeln
    # (config_root + state_root) und projektlokale .sepp via Trust-Registry, konsistent mit `sepp`.
    if [ -x "$target" ]; then
        if [ "$do_purge" = 1 ]; then
            "$target" uninstall --purge
        else
            "$target" uninstall
        fi
        return
    fi
    # Fallback (Binary schon weg): Datei-Reste anhand der aufgelösten Wurzeln aufräumen.
    config_dir="${SEPP_CONFIG_DIR:-${SEPP_HOME:-$HOME/.sepp}}"
    state_dir="${SEPP_STATE_DIR:-${SEPP_HOME:-$HOME/.sepp}}"
    log "Nicht gefunden (übersprungen): $target"
    if [ "$do_purge" = 1 ]; then
        for d in "$config_dir" "$state_dir"; do
            [ -d "$d" ] && rm -rf "$d" && log "Entfernt (--purge): $d"
        done
    else
        log "Hinweis: Nutzerdaten ($config_dir, $state_dir) bleiben erhalten."
        log "         Zum vollständigen Entfernen erneut mit --purge ausführen."
    fi
    log "Deinstallation abgeschlossen."
}

# Argumente einsammeln (echte Schleife, damit --uninstall --purge in beliebiger Reihenfolge geht).
mode=install
do_purge=0
do_from_source=0
do_system=0
while [ $# -gt 0 ]; do
    case "$1" in
        --from-source) do_from_source=1 ;;
        --system)      do_system=1 ;;
        --uninstall)   mode=uninstall ;;
        --purge)       do_purge=1 ;;
        -h|--help)     usage; exit 0 ;;
        *) die "unbekannte Option: $1 (erlaubt: --system, --from-source, --uninstall, --purge, --help)" ;;
    esac
    shift
done

[ "$do_purge" = 1 ] && [ "$mode" != uninstall ] && die "--purge ist nur zusammen mit --uninstall gültig"

# Binary-Zielverzeichnis: --system installiert systemweit (sofern SEPP_BIN_DIR nicht explizit gesetzt).
if [ -n "${SEPP_BIN_DIR:-}" ]; then
    BIN_DIR="$SEPP_BIN_DIR"
elif [ "$do_system" = 1 ]; then
    BIN_DIR="/usr/local/bin"
else
    BIN_DIR="$HOME/.local/bin"
fi

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

# --system: FHS-Layout direkt mit anlegen (ein Schritt).
if [ "$do_system" = 1 ]; then
    log "Richte System-Layout ein (sepp init --system) …"
    "${BIN_DIR}/sepp" init --system || die "sepp init --system fehlgeschlagen"
fi
