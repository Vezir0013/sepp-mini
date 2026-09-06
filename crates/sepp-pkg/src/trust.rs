//! Vertrauen in Herausgeber: TOFU (Trust On First Use) mit Bestätigung.
//!
//! Beim ersten Paket eines Herausgebers zeigt `install` Name und Fingerprint des Schlüssels; der
//! Nutzer bestätigt einmal. Danach liegt der Schlüssel unter `<state_root>/trusted-keys/<name>.json`
//! und jedes weitere Paket dieses Namens muss mit demselben Schlüssel signiert sein — ein anderer
//! Schlüssel unter bekanntem Namen ist ein Fehler, nie eine stille Ersetzung. Keine CA, kein
//! Web of Trust: eine Vertrauensentscheidung je Herausgeber, wie `trust.json` je Projekt.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use sepp_core::{Result, SeppError};
use sepp_policy::fsutil::{ensure_private_dir, write_atomic};

use crate::crypto::{decode_pubkey, fingerprint};
use crate::install::Roots;
use crate::manifest::Publisher;

/// Was `trusted-keys/<name>.json` enthält.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedKey {
    pub name: String,
    /// base64, wie im Manifest.
    pub key: String,
    pub fingerprint: String,
    /// Unix-Sekunden.
    pub trusted_at: u64,
    /// Wodurch das Vertrauen entstand (`install <datei>` oder `--trust-key`).
    pub via: String,
}

/// Ergebnis der Vertrauensprüfung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustStatus {
    /// Bekannt, Schlüssel passt.
    Known,
    /// Noch nie gesehen — Bestätigung nötig.
    New { name: String, fingerprint: String },
    /// Bekannt, aber der Schlüssel ist ein anderer.
    Mismatch {
        name: String,
        stored: String,
        offered: String,
    },
}

fn key_file(roots: &Roots, name: &str) -> PathBuf {
    roots.trusted_keys_dir().join(format!("{name}.json"))
}

/// Liest den gespeicherten Schlüssel eines Herausgebers, falls vorhanden.
pub fn stored_key(roots: &Roots, name: &str) -> Result<Option<TrustedKey>> {
    let path = key_file(roots, name);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(SeppError::Config(format!("pkg: {}: {e}", path.display()))),
    };
    let k: TrustedKey = serde_json::from_str(&text)
        .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", path.display())))?;
    Ok(Some(k))
}

/// Vergleicht den Herausgeber eines Pakets mit dem gespeicherten Schlüssel.
pub fn check_trust(roots: &Roots, publisher: &Publisher) -> Result<TrustStatus> {
    let offered = fingerprint(&decode_pubkey(&publisher.key)?);
    Ok(match stored_key(roots, &publisher.name)? {
        None => TrustStatus::New {
            name: publisher.name.clone(),
            fingerprint: offered,
        },
        Some(k) if k.key.trim() == publisher.key.trim() => TrustStatus::Known,
        Some(k) => TrustStatus::Mismatch {
            name: publisher.name.clone(),
            stored: k.fingerprint,
            offered,
        },
    })
}

/// Speichert den Schlüssel eines Herausgebers (Verzeichnis 0700, Datei 0600). Überschreibt
/// nie einen anderen Schlüssel — dafür gibt es [`untrust_publisher`] (`sepp pkg untrust`).
pub fn trust_publisher(roots: &Roots, publisher: &Publisher, via: &str) -> Result<TrustedKey> {
    if let Some(existing) = stored_key(roots, &publisher.name)? {
        if existing.key.trim() != publisher.key.trim() {
            return Err(SeppError::Config(format!(
                "pkg: Herausgeber {} ist mit einem anderen Schlüssel bekannt ({}) — zum \
                 Zurücknehmen `sepp pkg untrust {}`",
                publisher.name, existing.fingerprint, publisher.name
            )));
        }
        return Ok(existing);
    }
    let pubkey = decode_pubkey(&publisher.key)?;
    let entry = TrustedKey {
        name: publisher.name.clone(),
        key: publisher.key.trim().to_string(),
        fingerprint: fingerprint(&pubkey),
        trusted_at: now(),
        via: via.to_string(),
    };
    ensure_private_dir(&roots.trusted_keys_dir())?;
    let text = serde_json::to_string_pretty(&entry)
        .map_err(|e| SeppError::Config(format!("pkg: trusted-keys: {e}")))?;
    write_atomic(
        &key_file(roots, &publisher.name),
        text.as_bytes(),
        Some(0o600),
    )?;
    Ok(entry)
}

/// Ergebnis von [`untrust_publisher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Untrusted {
    pub name: String,
    pub fingerprint: String,
    /// Die gelöschte Datei.
    pub path: PathBuf,
    /// Installierte Pakete dieses Herausgebers — sie bleiben, nur genannt.
    pub installed: Vec<String>,
}

/// Nimmt das Vertrauen in einen Herausgeber zurück: löscht `trusted-keys/<name>.json`. Seine
/// installierten Pakete bleiben unangetastet; beim nächsten Paket dieses Namens wird der
/// Fingerprint wieder bestätigt.
pub fn untrust_publisher(roots: &Roots, name: &str) -> Result<Untrusted> {
    crate::validate_name(name)?;
    let Some(stored) = stored_key(roots, name)? else {
        return Err(SeppError::Config(format!(
            "pkg: für Herausgeber {name} ist kein Schlüssel gespeichert"
        )));
    };
    let installed: Vec<String> = crate::install::Installed::load(roots)?
        .packages
        .iter()
        .filter(|(_, e)| e.publisher == name)
        .map(|(n, _)| n.clone())
        .collect();
    let path = key_file(roots, name);
    std::fs::remove_file(&path)
        .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", path.display())))?;
    Ok(Untrusted {
        name: name.to_string(),
        fingerprint: stored.fingerprint,
        path,
        installed,
    })
}

/// Alle gespeicherten Herausgeber (für `sepp pkg list`).
pub fn trusted_publishers(roots: &Roots) -> Result<Vec<TrustedKey>> {
    let dir = roots.trusted_keys_dir();
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(SeppError::Config(format!("pkg: {}: {e}", dir.display()))),
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(k) = serde_json::from_str::<TrustedKey>(&text) {
                out.push(k);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{encode_pubkey, SigningKey};

    fn roots(tmp: &std::path::Path) -> Roots {
        Roots {
            config: tmp.join("config"),
            state: tmp.join("state"),
        }
    }

    fn publisher(name: &str, key: &SigningKey) -> Publisher {
        Publisher {
            name: name.into(),
            key: encode_pubkey(&key.public_key()),
        }
    }

    #[test]
    fn first_use_is_new_then_known_and_a_second_key_is_a_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let r = roots(tmp.path());
        let (k1, _) = SigningKey::generate().unwrap();
        let (k2, _) = SigningKey::generate().unwrap();
        let p1 = publisher("acme", &k1);

        assert_eq!(
            check_trust(&r, &p1).unwrap(),
            TrustStatus::New {
                name: "acme".into(),
                fingerprint: k1.fingerprint()
            }
        );
        let stored = trust_publisher(&r, &p1, "install demo.seppkg").unwrap();
        assert_eq!(stored.fingerprint, k1.fingerprint());
        assert_eq!(check_trust(&r, &p1).unwrap(), TrustStatus::Known);

        let p2 = publisher("acme", &k2);
        assert_eq!(
            check_trust(&r, &p2).unwrap(),
            TrustStatus::Mismatch {
                name: "acme".into(),
                stored: k1.fingerprint(),
                offered: k2.fingerprint()
            }
        );
        assert!(trust_publisher(&r, &p2, "x").is_err(), "nie überschreiben");
        assert_eq!(check_trust(&r, &p1).unwrap(), TrustStatus::Known);

        let all = trusted_publishers(&r).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "acme");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = r.trusted_keys_dir();
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            let f = dir.join("acme.json");
            assert_eq!(
                std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
