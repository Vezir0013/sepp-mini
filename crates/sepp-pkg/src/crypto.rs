//! Hashing, Signatur und Schlüssel — alles über `ring`, das über `rustls` ohnehin im Baum liegt.
//!
//! SHA-256 je Datei, Ed25519 über das Manifest. Der Fingerprint eines Herausgebers sind die
//! ersten 16 Hex-Zeichen von SHA-256 seines Public Keys — kurz genug zum Vorlesen, lang genug,
//! um nicht zu kollidieren. Fehler aus `ring` (`Unspecified`) werden immer mit Kontext gemappt.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair as _, UnparsedPublicKey, ED25519};

use sepp_core::{Result, SeppError};
use sepp_policy::fsutil::write_atomic;

use crate::hex;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// SHA-256 als 64 Hex-Zeichen.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// Streaming-Hasher für Dateien, die beim Entpacken durchlaufen.
pub struct Hasher(ring::digest::Context);

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher {
    pub fn new() -> Self {
        Hasher(ring::digest::Context::new(&ring::digest::SHA256))
    }
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
    pub fn finish_hex(self) -> String {
        hex(self.0.finish().as_ref())
    }
}

/// Fingerprint eines Public Keys: erste 16 Hex von SHA-256.
pub fn fingerprint(pubkey: &[u8]) -> String {
    sha256_hex(pubkey)[..16].to_string()
}

/// Dekodiert einen base64-Public-Key; muss genau 32 Byte ergeben.
pub fn decode_pubkey(b64: &str) -> Result<Vec<u8>> {
    let bytes = B64
        .decode(b64.trim())
        .map_err(|_| SeppError::Config("pkg: Schlüssel ist kein gültiges Base64".into()))?;
    if bytes.len() != 32 {
        return Err(SeppError::Config(format!(
            "pkg: Schlüssel hat {} Byte, ein Ed25519-Public-Key hat 32",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Public Key als base64 (so steht er im Manifest).
pub fn encode_pubkey(pubkey: &[u8]) -> String {
    B64.encode(pubkey)
}

/// Signatur als base64 (so steht sie in `manifest.sig`).
pub fn encode_signature(sig: &[u8]) -> String {
    B64.encode(sig)
}

/// Dekodiert `manifest.sig`; muss 64 Byte ergeben.
pub fn decode_signature(b64: &str) -> Result<Vec<u8>> {
    let bytes = B64
        .decode(b64.trim())
        .map_err(|_| SeppError::Config("pkg: Signatur ist kein gültiges Base64".into()))?;
    if bytes.len() != 64 {
        return Err(SeppError::Config(format!(
            "pkg: Signatur hat {} Byte, eine Ed25519-Signatur hat 64",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Prüft eine Ed25519-Signatur. Ein Fehler heißt: nicht von diesem Schlüssel, oder verändert.
pub fn verify(pubkey: &[u8], msg: &[u8], sig: &[u8]) -> Result<()> {
    UnparsedPublicKey::new(&ED25519, pubkey)
        .verify(msg, sig)
        .map_err(|_| {
            SeppError::Config(
                "pkg: Signatur ungültig — das Manifest wurde verändert oder stammt nicht vom \
                 angegebenen Herausgeber"
                    .into(),
            )
        })
}

/// Der geheime Schlüssel eines Herausgebers.
pub struct SigningKey(Ed25519KeyPair);

impl SigningKey {
    /// Erzeugt ein Schlüsselpaar; liefert es samt PKCS#8-DER zum Speichern.
    pub fn generate() -> Result<(Self, Vec<u8>)> {
        let der = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| SeppError::Config("pkg: Schlüssel erzeugen fehlgeschlagen".into()))?;
        let key = Self::from_pkcs8(der.as_ref())?;
        Ok((key, der.as_ref().to_vec()))
    }

    pub fn from_pkcs8(der: &[u8]) -> Result<Self> {
        Ed25519KeyPair::from_pkcs8(der)
            .map(SigningKey)
            .map_err(|e| SeppError::Config(format!("pkg: Schlüssel unlesbar: {e}")))
    }

    /// Der Public Key (32 Byte).
    pub fn public_key(&self) -> Vec<u8> {
        self.0.public_key().as_ref().to_vec()
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public_key())
    }

    /// Signiert `msg` (64 Byte).
    pub fn sign(&self, msg: &[u8]) -> Vec<u8> {
        self.0.sign(msg).as_ref().to_vec()
    }
}

/// Wo ein Herausgeber-Schlüsselpaar liegt: `publisher.key` (base64 PKCS#8, 0600) und
/// `publisher.pub` (base64 Public Key).
#[derive(Debug, Clone)]
pub struct KeyFiles {
    pub secret: PathBuf,
    pub public: PathBuf,
}

impl KeyFiles {
    /// Beide Dateien in `dir`.
    pub fn in_dir(dir: &Path) -> Self {
        KeyFiles {
            secret: dir.join("publisher.key"),
            public: dir.join("publisher.pub"),
        }
    }

    /// Das Schlüsselpaar eines Registry-Betreibers in `dir` (`registry.key`/`registry.pub`).
    pub fn registry_in_dir(dir: &Path) -> Self {
        KeyFiles {
            secret: dir.join("registry.key"),
            public: dir.join("registry.pub"),
        }
    }
}

/// Erzeugt ein neues Schlüsselpaar und schreibt es — nie über ein vorhandenes hinweg.
/// Liefert den Fingerprint.
pub fn write_new_keypair(files: &KeyFiles) -> Result<String> {
    if files.secret.exists() || files.public.exists() {
        return Err(SeppError::Config(format!(
            "pkg: {} existiert schon — ein Schlüssel wird nie überschrieben (zum Neuanlegen \
             erst von Hand entfernen)",
            files.secret.display()
        )));
    }
    let (key, der) = SigningKey::generate()?;
    if let Some(parent) = files.secret.parent() {
        sepp_policy::fsutil::ensure_private_dir(parent)?;
    }
    write_atomic(
        &files.secret,
        format!("{}\n", B64.encode(&der)).as_bytes(),
        Some(0o600),
    )?;
    write_atomic(
        &files.public,
        format!("{}\n", encode_pubkey(&key.public_key())).as_bytes(),
        Some(0o644),
    )?;
    Ok(key.fingerprint())
}

/// Lädt den geheimen Schlüssel aus `publisher.key`.
pub fn load_signing_key(path: &Path) -> Result<SigningKey> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", path.display())))?;
    let der = B64
        .decode(text.trim())
        .map_err(|_| SeppError::Config(format!("pkg: {}: kein Base64", path.display())))?;
    SigningKey::from_pkcs8(&der)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_known_empty_hash() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let mut h = Hasher::new();
        h.update(b"hal");
        h.update(b"lo");
        assert_eq!(h.finish_hex(), sha256_hex(b"hallo"));
    }

    #[test]
    fn sign_verify_roundtrip_and_tamper_detection() {
        let (key, der) = SigningKey::generate().unwrap();
        let again = SigningKey::from_pkcs8(&der).unwrap();
        assert_eq!(key.public_key(), again.public_key());
        let msg = b"name = \"x\"\n";
        let sig = key.sign(msg);
        assert_eq!(sig.len(), 64);
        verify(&key.public_key(), msg, &sig).unwrap();
        assert!(verify(&key.public_key(), b"name = \"y\"\n", &sig).is_err());
        let (other, _) = SigningKey::generate().unwrap();
        assert!(verify(&other.public_key(), msg, &sig).is_err());
        // base64 hin und zurück
        assert_eq!(decode_signature(&encode_signature(&sig)).unwrap(), sig);
        assert_eq!(
            decode_pubkey(&encode_pubkey(&key.public_key())).unwrap(),
            key.public_key()
        );
        assert!(decode_pubkey("AAAA").is_err());
        assert!(decode_signature("AAAA").is_err());
    }

    #[test]
    fn fingerprint_is_16_hex_and_stable() {
        let fp = fingerprint(&[0u8; 32]);
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(fp, fingerprint(&[0u8; 32]));
        assert_ne!(fp, fingerprint(&[1u8; 32]));
    }

    #[test]
    fn keypair_files_are_written_once_and_load_again() {
        let dir = tempfile::tempdir().unwrap();
        let files = KeyFiles::in_dir(&dir.path().join("keys"));
        let fp = write_new_keypair(&files).unwrap();
        let key = load_signing_key(&files.secret).unwrap();
        assert_eq!(key.fingerprint(), fp);
        let pub_text = std::fs::read_to_string(&files.public).unwrap();
        assert_eq!(decode_pubkey(&pub_text).unwrap(), key.public_key());
        assert!(write_new_keypair(&files).is_err(), "nie überschreiben");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&files.secret)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
