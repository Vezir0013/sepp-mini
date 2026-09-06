//! `sepp-pkg` — das Paketformat `.seppkg` und seine Installation.
//!
//! Ein Paket bündelt Skills, Prompt-Templates, Hooks und WASM-Plugins, ist vom Herausgeber
//! signiert und lässt sich mit `sepp pkg install` in einem Schritt installieren. Die Rechte, die
//! seine Plugins brauchen, **bringt es nicht mit** — es bittet um sie: Bei der Installation werden
//! sie gezeigt, der Nutzer stimmt zu, und erst dann schreibt `sepp` sie als markierten Block in
//! die eigene `policy.toml` (siehe `sepp_policy::policy_edit`). `sepp pkg remove` nimmt genau
//! diesen Block wieder heraus.
//!
//! **Leitsätze**
//! - Eine Rechtequelle: kein Paket enthält eine `policy.toml` oder `settings.toml`.
//! - `skills/`, `prompts/`, `hooks/`, `plugins/` gehören dem Nutzer; Pakete leben unter
//!   `<config_root>/pkg/<name>/` und werden von den Loadern wie eine weitere Wurzel gelesen.
//! - Content in `config_root`, Nachweise (`installed.json`, Herausgeber-Schlüssel) in
//!   `state_root`, mit `0600`/`0700`.
//! - Alles additiv: `format = 1`; unbekannte Manifest-Felder werden gelesen, gemeldet, ignoriert.
//!
//! **Format 1.** Ein `.seppkg` ist ein zstd-komprimiertes tar. Die ersten beiden Einträge sind
//! `manifest.toml` und `manifest.sig` (Ed25519 über die rohen Bytes des Manifests, base64), dann
//! folgen die Dateien in sortierter Reihenfolge. Das Manifest nennt je Datei ihren SHA-256 — eine
//! Signatur deckt so das ganze Paket, und die Prüfung je Datei ist ein Hash-Vergleich. Erlaubte
//! Top-Level-Einträge: `skills/`, `prompts/`, `hooks/`, `plugins/` (je `<n>.wasm` + `<n>.toml`),
//! optional `README.md` und `LICENSE`.
//!
//! **Prüfreihenfolge bei der Installation** (fail-closed, vor der Signaturprüfung landet kein
//! Nutzdaten-Byte auf Platte): Magic → Manifest und Signatur zuerst im Archiv → Signatur gegen
//! den Schlüssel im Manifest → [`PkgManifest::validate`] → Vertrauen in den Herausgeber (TOFU:
//! beim ersten Mal Fingerprint bestätigen, danach muss der Schlüssel passen) → Variablen →
//! `[rights]` gegen das Plugin-Manifest → Zustimmung → Kollisionen → Entpacken mit Hash je Datei
//! (keine Datei ohne Eintrag, kein Eintrag ohne Datei, keine Symlinks, keine `..`).
//!
//! **Registry** ([`registry`]): `sepp pkg install <name>` holt ein Paket aus einem signierten
//! Index (`index.toml` + `index.sig`, Schlüssel des Betreibers gepinnt in `settings.toml`
//! `[[registries]]`). Ein Index nennt Pakete, er gewährt nichts — Herausgeber-TOFU und
//! Zustimmung bleiben beim Paket. Netz gibt es nur über [`registry::Fetcher`], den das CLI stellt.
//!
//! Die Krypto kommt aus `ring` (liegt über `rustls` ohnehin im Baum): SHA-256, Ed25519,
//! `SystemRandom`. Dieses Crate macht Datei-I/O, aber kein Netz und kein async.

pub mod container;
pub mod crypto;
pub mod install;
pub mod manifest;
pub mod registry;
pub mod trust;
pub mod vars;

pub use container::{pack_dir, ExtractReport, PackReport, PkgArchive, Signed};
pub use crypto::{fingerprint, sha256_hex, KeyFiles, SigningKey};
pub use install::{
    apply_install, check_collisions, check_rights, consent_lines, list, package_dirs_in,
    plan_install, remove, Collisions, InstallPlan, InstallReport, Installed, InstalledEntry,
    Listed, RemoveReport, Roots,
};
pub use manifest::{Inventory, PkgManifest, Publisher, VarKind, VarSpec};
pub use registry::{
    build_index, check_url_scheme, download_package, fetch_index, join_url, load_registries,
    parse_registries, parse_spec, verify_index, Fetcher, Index, IndexBuild, IndexEntry, PkgSpec,
    RegistryConfig,
};
pub use trust::{
    check_trust, trust_publisher, trusted_publishers, untrust_publisher, TrustStatus, TrustedKey,
    Untrusted,
};
pub use vars::{resolve_rights, resolve_vars, substitute, value_notes, Resolved};

use sepp_core::{Result, SeppError};

/// Formatversion, die dieser Host liest und schreibt.
pub const FORMAT: u32 = 1;

/// Dateiendung eines Pakets.
pub const EXTENSION: &str = "seppkg";

/// Größtes Manifest im Archiv.
pub const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
/// Größte einzelne Datei im Paket.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Größe aller Dateien zusammen (entpackt).
pub const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// Höchste Zahl an Archiveinträgen.
pub const MAX_ENTRIES: usize = 2000;
/// Tiefste erlaubte Verzeichnisschachtelung eines Paketpfads.
pub const MAX_DEPTH: usize = 8;

/// Top-Level-Verzeichnisse, die ein Paket enthalten darf — dieselben, die auch der Nutzer hat.
pub const CONTENT_DIRS: &[&str] = &["skills", "prompts", "hooks", "plugins"];
/// Top-Level-Dateien, die ein Paket zusätzlich enthalten darf.
pub const CONTENT_FILES: &[&str] = &["README.md", "LICENSE"];

/// Namensregel für Pakete und Herausgeber: `^[a-z][a-z0-9_]{0,63}$`. Der Name ist zugleich
/// Verzeichnisname unter `pkg/` bzw. Dateiname unter `trusted-keys/`.
pub fn validate_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let ok_first = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    let ok_rest = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ok_first || !ok_rest || name.len() > 64 {
        return Err(SeppError::Config(format!(
            "pkg: Name {name:?} ist unzulässig — erlaubt sind 1 bis 64 Zeichen aus a-z, 0-9 und _, \
             beginnend mit einem Buchstaben"
        )));
    }
    Ok(())
}

/// Ein Paketpfad ist relativ, ohne `.`/`..`, ohne Backslash, höchstens [`MAX_DEPTH`] tief und
/// beginnt mit einem erlaubten Top-Level-Eintrag. Alles andere wäre ein Weg aus dem
/// Paketverzeichnis heraus oder in eines, das dem Nutzer gehört.
pub fn validate_rel_path(path: &str) -> Result<()> {
    if path.is_empty() || path.contains('\\') || path.contains('\0') {
        return Err(SeppError::Config(format!("pkg: ungültiger Pfad {path:?}")));
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > MAX_DEPTH {
        return Err(SeppError::Config(format!(
            "pkg: Pfad {path:?} ist tiefer als {MAX_DEPTH} Ebenen"
        )));
    }
    for part in &parts {
        if part.is_empty() || *part == "." || *part == ".." {
            return Err(SeppError::Config(format!(
                "pkg: Pfad {path:?} ist nicht relativ oder enthält `.`/`..`"
            )));
        }
    }
    let top = parts[0];
    let ok = if parts.len() == 1 {
        CONTENT_FILES.contains(&top)
    } else {
        CONTENT_DIRS.contains(&top)
    };
    if !ok {
        return Err(SeppError::Config(format!(
            "pkg: {path:?} liegt außerhalb von {} (Dateien: {})",
            CONTENT_DIRS.join("/, ") + "/",
            CONTENT_FILES.join(", ")
        )));
    }
    Ok(())
}

/// Der Text eines Fehlers ohne die Präfixe `config error: ` und `pkg: ` — zum Einbetten in eine
/// Meldung, die selbst wieder ein `SeppError::Config` wird.
pub(crate) fn plain(e: &SeppError) -> String {
    let s = e.to_string();
    let s = s.strip_prefix("config error: ").unwrap_or(&s);
    s.strip_prefix("pkg: ").unwrap_or(s).to_string()
}

/// Kleines Hex ohne Extra-Crate — Hashes bleiben mit `sha256sum` vergleichbar.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_rule() {
        for ok in ["a", "rechnung", "pdf_extract2", &"x".repeat(64)] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Rechnung", "1a", "a-b", "a.b", "grüße", &"x".repeat(65)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn paths_stay_inside_the_package() {
        for ok in [
            "skills/x/SKILL.md",
            "skills/kurz.md",
            "prompts/review.md",
            "hooks/a.rhai",
            "plugins/p.wasm",
            "README.md",
            "LICENSE",
        ] {
            assert!(validate_rel_path(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "skills/../../x",
            "skills/./x",
            "skills\\x",
            "policy.toml",
            "settings.toml",
            "plugins",
            "other/x",
            "skills/a/b/c/d/e/f/g/h/i.md",
        ] {
            assert!(validate_rel_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(hex(&[0, 255, 16]), "00ff10");
        assert_eq!(unhex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(unhex("0"), None);
        assert_eq!(unhex("zz"), None);
    }
}
