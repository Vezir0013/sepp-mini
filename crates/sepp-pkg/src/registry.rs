//! Registry: ein signierter Index, aus dem `sepp pkg install <name>` ein Paket beim Namen holt.
//!
//! Ein Index **nennt** Pakete, er gewährt nichts: Rechte werden weiterhin beim Paket erbeten,
//! das Vertrauen in den Herausgeber bleibt TOFU. Der Index selbst ist vom Betreiber signiert
//! (Ed25519 über die rohen Bytes von `index.toml`, base64 in `index.sig`), und dessen Public Key
//! steht **gepinnt** in der `settings.toml` des Nutzers (`[[registries]]`) — kein TOFU für den
//! Index. Zwei Lagen: der Betreiber bürgt für die Liste, der Herausgeber für das Paket. Beim
//! Installieren muss der Herausgeber-Schlüssel aus dem Index zu dem im Paket passen.
//!
//! Dieses Modul kennt kein Netz. Alles, was Bytes holt, läuft über [`Fetcher`] — das CLI stellt
//! die HTTP-Implementierung, Tests einen Fake. Die Schema-Regel ([`check_url_scheme`]) gilt für
//! jede URL, die hier vorkommt: `https://` immer, `http://` nur für Loopback.
//!
//! **Index-Format 1**
//!
//! ```toml
//! format = 1
//! name = "kionova"                       # informativ
//! generated_at = 1788700000              # Unix-Sekunden, informativ
//! [[packages]]
//! name = "rechnungspruefung"
//! version = "1.0.0"
//! description = "…"
//! publisher = "acme"                     # aus dem Paket-Manifest
//! publisher_key = "<base64 32 Byte>"     # aus dem Paket-Manifest
//! url = "rechnungspruefung-1.0.0.seppkg" # relativ zur Index-URL oder absolut
//! sha256 = "<64 hex>"                    # über die .seppkg-Datei
//! size = 44840                           # Bytes der .seppkg-Datei
//! ```
//!
//! **Prüfreihenfolge beim Lesen** (fail-closed): Größe → Signatur gegen den gepinnten Schlüssel →
//! UTF-8 → parse → [`Index::validate`]. Erst nach der Signatur wird der Text überhaupt
//! angesehen. Ein Paket aus dem Index wird gestreamt in eine Datei geladen, dabei gedeckelt
//! (`size` aus dem Index) und gehasht; stimmt Größe oder SHA-256 nicht, ist die Datei weg —
//! danach geht es den normalen Weg von `sepp pkg install <datei>`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use sepp_core::is_display_safe;
use sepp_core::{Result, SeppError};
use sepp_policy::fsutil::ensure_private_dir;
use sepp_policy::url_host;

use crate::container::{hash_file, PkgArchive};
use crate::crypto::{
    decode_pubkey, decode_signature, encode_pubkey, encode_signature, verify, Hasher, SigningKey,
};
use crate::trust::now;
use crate::{plain, unhex, validate_name, EXTENSION, MAX_TOTAL_BYTES};

/// Formatversion des Index, die dieser Host liest und schreibt.
pub const INDEX_FORMAT: u32 = 1;
/// Größter Index, der geladen wird.
pub const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;
/// Größte Signaturdatei (base64 von 64 Byte plus Zeilenende, großzügig).
pub const MAX_SIG_BYTES: u64 = 256;
/// Dateiname des Index neben den Paketen.
pub const INDEX_FILE: &str = "index.toml";
/// Dateiname der Signatur neben dem Index.
pub const SIG_FILE: &str = "index.sig";

fn index_v1() -> u32 {
    1
}

/// Der Index einer Registry.
#[derive(Debug, Clone, Deserialize)]
pub struct Index {
    #[serde(default = "index_v1")]
    pub format: u32,
    /// Name, den der Betreiber vergibt — informativ, wird nicht mit dem Alias in
    /// `settings.toml` verglichen (die Bindung ist der Schlüssel).
    pub name: String,
    /// Unix-Sekunden, informativ.
    #[serde(default)]
    pub generated_at: u64,
    #[serde(default)]
    pub packages: Vec<IndexEntry>,
    /// Unbekannte Felder: gelesen, gemeldet, ignoriert.
    #[serde(flatten)]
    pub unknown: BTreeMap<String, toml::Value>,
}

/// Ein Paket im Index — eine Version. Mehrere Einträge je Name sind erlaubt.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IndexEntry {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Herausgeber-Name aus dem Paket-Manifest.
    pub publisher: String,
    /// Herausgeber-Schlüssel (base64) aus dem Paket-Manifest — muss beim Installieren zum
    /// Schlüssel im Paket passen.
    pub publisher_key: String,
    /// Relativ zur Index-URL (Regeln in [`join_url`]) oder absolut ([`check_url_scheme`]).
    pub url: String,
    /// SHA-256 der `.seppkg`-Datei, 64 Hex.
    pub sha256: String,
    /// Bytes der `.seppkg`-Datei — zugleich der Deckel beim Laden.
    pub size: u64,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, toml::Value>,
}

/// Ein Eintrag `[[registries]]` aus der `settings.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RegistryConfig {
    /// Alias beim Nutzer (`--registry <name>`), Namensregel wie bei Paketen.
    pub name: String,
    /// URL des Index (`…/index.toml`); die Signatur liegt unter [`sig_url`].
    pub url: String,
    /// Public Key des Betreibers, base64 — gepinnt, kein TOFU.
    pub key: String,
}

/// `name[@version]`, wie `sepp pkg install` es entgegennimmt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkgSpec {
    pub name: String,
    /// Normalisierte semver-Version, falls angegeben.
    pub version: Option<String>,
}

/// Ergebnis von [`build_index`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexBuild {
    pub index_text: String,
    /// Inhalt für `index.sig` (base64 + Zeilenende).
    pub signature_text: String,
    /// `(name, version)` je Eintrag, sortiert.
    pub entries: Vec<(String, String)>,
    pub fingerprint: String,
    /// Public Key base64 — das, was Nutzer als `key` in `[[registries]]` eintragen.
    pub public_key: String,
    pub warnings: Vec<String>,
}

/// Was Bytes von einer URL holt. Das CLI implementiert es mit HTTP; Tests mit einer Tabelle.
///
/// Verträge: `max` ist ein harter Deckel — die Implementierung liefert nie mehr Bytes an `out`
/// und bricht möglichst **vor** dem Lesen ab (Content-Length), spätestens beim Streamen. Ein
/// Status außerhalb 2xx ist ein Fehler. Die Schema-Regel ([`check_url_scheme`]) prüft die
/// Implementierung für die URL und jedes Redirect-Ziel selbst noch einmal.
pub trait Fetcher {
    /// Holt `url` und schreibt den Body nach `out`; liefert die Zahl der geschriebenen Bytes.
    fn fetch_to_writer(&self, url: &str, max: u64, out: &mut dyn Write) -> Result<u64>;

    /// Holt `url` in den Speicher (für Index und Signatur).
    fn fetch(&self, url: &str, max: u64) -> Result<Vec<u8>> {
        let mut buf: Vec<u8> = Vec::new();
        let n = self.fetch_to_writer(url, max, &mut buf)?;
        if n > max || buf.len() as u64 > max {
            return Err(SeppError::Config(format!(
                "pkg: {url}: mehr als {max} Bytes"
            )));
        }
        Ok(buf)
    }
}

impl Index {
    /// Parst den Text, ohne zu validieren.
    pub fn parse(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| SeppError::Config(format!("pkg: Index: {e}")))
    }

    /// Unbekannte Felder oben und je Eintrag (`packages[<name>@<version>].<feld>`).
    pub fn unknown_keys(&self) -> Vec<String> {
        let mut out: Vec<String> = self.unknown.keys().cloned().collect();
        for e in &self.packages {
            for k in e.unknown.keys() {
                out.push(format!("packages[{}@{}].{k}", e.name, e.version));
            }
        }
        out
    }

    /// Prüft alles, was sich ohne Netz prüfen lässt. Fail-closed.
    pub fn validate(&self) -> Result<()> {
        if self.format > INDEX_FORMAT {
            return Err(SeppError::Config(format!(
                "pkg: Index hat Format {}, dieser sepp liest {INDEX_FORMAT} — neueres sepp nötig",
                self.format
            )));
        }
        if self.format == 0 {
            return Err(SeppError::Config(
                "pkg: Index: `format` muss ≥ 1 sein".into(),
            ));
        }
        validate_name(&self.name)
            .map_err(|e| SeppError::Config(format!("pkg: {} (Index: name)", plain(&e))))?;
        let mut seen: BTreeSet<(String, semver::Version)> = BTreeSet::new();
        for e in &self.packages {
            let at = |what: &str, msg: String| {
                SeppError::Config(format!(
                    "pkg: Index, Eintrag {}@{}: {what} {msg}",
                    e.name, e.version
                ))
            };
            validate_name(&e.name).map_err(|err| {
                SeppError::Config(format!("pkg: {} (Index: packages.name)", plain(&err)))
            })?;
            let version = semver::Version::parse(&e.version)
                .map_err(|err| at("version", format!("ist keine semver-Version ({err})")))?;
            validate_name(&e.publisher).map_err(|err| {
                SeppError::Config(format!("pkg: {} (Index: packages.publisher)", plain(&err)))
            })?;
            decode_pubkey(&e.publisher_key).map_err(|err| at("publisher_key:", plain(&err)))?;
            if e.sha256.len() != 64 || unhex(&e.sha256).is_none() {
                return Err(at("sha256", "muss 64 Hex-Zeichen haben".into()));
            }
            if e.size == 0 || e.size > MAX_TOTAL_BYTES {
                return Err(at(
                    "size",
                    format!("muss zwischen 1 und {MAX_TOTAL_BYTES} liegen"),
                ));
            }
            validate_entry_url(&e.url).map_err(|err| at("url:", plain(&err)))?;
            if !seen.insert((e.name.clone(), version)) {
                return Err(at("", "kommt doppelt vor".into()));
            }
        }
        Ok(())
    }

    /// Der Eintrag zu `name`: die exakte `version`, sonst die höchste. Der Fehlertext nennt die
    /// vorhandenen Versionen.
    pub fn resolve(&self, name: &str, version: Option<&str>) -> Result<&IndexEntry> {
        let want = match version {
            Some(v) => Some(semver::Version::parse(v).map_err(|e| {
                SeppError::Config(format!("pkg: Version {v:?} ist keine semver-Version ({e})"))
            })?),
            None => None,
        };
        let mut best: Option<(&IndexEntry, semver::Version)> = None;
        let mut available: Vec<String> = Vec::new();
        for e in self.packages.iter().filter(|e| e.name == name) {
            let Ok(v) = semver::Version::parse(&e.version) else {
                continue;
            };
            available.push(v.to_string());
            match &want {
                Some(w) if *w == v => return Ok(e),
                Some(_) => {}
                None => {
                    if best.as_ref().is_none_or(|(_, b)| v > *b) {
                        best = Some((e, v));
                    }
                }
            }
        }
        if available.is_empty() {
            return Err(SeppError::Config(format!("pkg: {name} ist nicht im Index")));
        }
        match (want, best) {
            (Some(w), _) => Err(SeppError::Config(format!(
                "pkg: {name} {w} ist nicht im Index (vorhanden: {})",
                available.join(", ")
            ))),
            (None, Some((e, _))) => Ok(e),
            (None, None) => Err(SeppError::Config(format!("pkg: {name} ist nicht im Index"))),
        }
    }

    /// Für `search`: je Name nur die höchste Version, gefiltert auf `text` (Substring, ohne
    /// Groß/Klein) in Name oder Beschreibung, sortiert nach Name.
    pub fn matching(&self, text: Option<&str>) -> Vec<&IndexEntry> {
        let needle = text.map(|t| t.to_lowercase()).filter(|t| !t.is_empty());
        let mut latest: BTreeMap<&str, (&IndexEntry, semver::Version)> = BTreeMap::new();
        for e in &self.packages {
            let Ok(v) = semver::Version::parse(&e.version) else {
                continue;
            };
            match latest.get(e.name.as_str()) {
                Some((_, have)) if *have >= v => {}
                _ => {
                    latest.insert(e.name.as_str(), (e, v));
                }
            }
        }
        latest
            .into_values()
            .map(|(e, _)| e)
            .filter(|e| match &needle {
                None => true,
                Some(n) => {
                    e.name.to_lowercase().contains(n.as_str())
                        || e.description
                            .as_deref()
                            .is_some_and(|d| d.to_lowercase().contains(n.as_str()))
                }
            })
            .collect()
    }
}

/// Prüft Größe, Signatur, Text und Struktur eines Index — in dieser Reihenfolge.
pub fn verify_index(index_bytes: &[u8], sig_text: &str, key_b64: &str) -> Result<Index> {
    if index_bytes.len() as u64 > MAX_INDEX_BYTES {
        return Err(SeppError::Config(format!(
            "pkg: Index ist größer als {MAX_INDEX_BYTES} Bytes"
        )));
    }
    let key = decode_pubkey(key_b64).map_err(|e| {
        SeppError::Config(format!(
            "pkg: {} (Registry-Schlüssel in settings.toml)",
            plain(&e)
        ))
    })?;
    let sig = decode_signature(sig_text)
        .map_err(|e| SeppError::Config(format!("pkg: {} ({SIG_FILE})", plain(&e))))?;
    verify(&key, index_bytes, &sig).map_err(|_| {
        SeppError::Config(
            "pkg: Index-Signatur ungültig — der Index wurde verändert oder stammt nicht vom \
             Betreiber, dessen Schlüssel in settings.toml steht"
                .into(),
        )
    })?;
    let text = std::str::from_utf8(index_bytes)
        .map_err(|_| SeppError::Config("pkg: Index ist kein UTF-8".into()))?;
    let index = Index::parse(text)?;
    index.validate()?;
    Ok(index)
}

/// `localhost`, `127.0.0.1`, `::1` — die Hosts, für die `http://` erlaubt ist.
pub fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Die Schema-Regel: `https://` immer, `http://` nur für Loopback, alles andere nie.
///
/// Vorgeschaltet die Zeichenprüfung. Sie ist zum Teil Verteidigung in der Tiefe —
/// [`url_host`] lehnt Whitespace und Steuerzeichen schon ab, aber das ist eine *geerbte*
/// Zusage, die still wegfiele, wenn jemand dort etwas umbaut; hier steht sie ausgesprochen.
/// Zum anderen Teil schließt sie eine echte Lücke: `is_control()` kennt nur die
/// Steuerzeichen, nicht die Formatzeichen. Ein Hostname mit einem Rechts-nach-links-Zeichen
/// käme durch und erschiene im Zustimmungsdialog als ein ganz anderer.
pub fn check_url_scheme(url: &str) -> Result<()> {
    if !is_display_safe(url) || url.chars().any(char::is_whitespace) {
        return Err(SeppError::Config(format!(
            "pkg: {url:?}: URL mit Whitespace, Steuer- oder Formatzeichen"
        )));
    }
    if url.starts_with("https://") {
        if url_host(url).is_some() {
            return Ok(());
        }
        return Err(SeppError::Config(format!("pkg: {url:?}: URL ohne Host")));
    }
    if url.starts_with("http://") {
        if url_host(url).is_some_and(|h| is_loopback(&h)) {
            return Ok(());
        }
        return Err(SeppError::Config(format!(
            "pkg: {url:?}: http:// nur für localhost, 127.0.0.1 oder ::1 — sonst https://"
        )));
    }
    Err(SeppError::Config(format!(
        "pkg: {url:?}: nur https:// (http:// nur für Loopback)"
    )))
}

/// Eine relative Paket-URL: ein Pfad ohne Query, Fragment, Backslash, führenden `/` und ohne
/// `.`/`..`-Segmente.
fn validate_rel_url(rel: &str) -> Result<()> {
    let bad = rel.is_empty()
        || rel.starts_with('/')
        || rel.contains(['?', '#', '\\'])
        || rel.chars().any(|c| c.is_whitespace() || c.is_control())
        || rel
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..");
    if bad {
        return Err(SeppError::Config(format!(
            "pkg: {rel:?} ist keine zulässige relative URL (ohne `..`, `?`, `#`, führenden `/`)"
        )));
    }
    Ok(())
}

/// Absolut nach der Schema-Regel oder relativ nach [`validate_rel_url`].
fn validate_entry_url(url: &str) -> Result<()> {
    if url.contains("://") {
        check_url_scheme(url)
    } else {
        validate_rel_url(url)
    }
}

/// Löst die Paket-URL eines Eintrags gegen die Index-URL auf: absolut bleibt absolut (nach der
/// Schema-Regel), relativ hängt an die Index-URL bis einschließlich ihrem letzten `/`.
pub fn join_url(index_url: &str, rel: &str) -> Result<String> {
    if rel.contains("://") {
        check_url_scheme(rel)?;
        return Ok(rel.to_string());
    }
    validate_rel_url(rel)?;
    check_url_scheme(index_url)?;
    let scheme_end = index_url.find("://").map(|i| i + 3).unwrap_or(0);
    let base = match index_url[scheme_end..].rfind('/') {
        Some(i) => index_url[..scheme_end + i + 1].to_string(),
        None => format!("{index_url}/"),
    };
    Ok(format!("{base}{rel}"))
}

/// Wo die Signatur zum Index liegt: `….toml` → `….sig`, sonst `.sig` angehängt.
pub fn sig_url(index_url: &str) -> String {
    match index_url.strip_suffix(".toml") {
        Some(base) => format!("{base}.sig"),
        None => format!("{index_url}.sig"),
    }
}

/// Zerlegt `name[@version]`; die Version wird über semver normalisiert.
pub fn parse_spec(spec: &str) -> Result<PkgSpec> {
    let (name, version) = match spec.split_once('@') {
        Some((n, v)) => (n, Some(v)),
        None => (spec, None),
    };
    validate_name(name)?;
    let version = match version {
        Some(v) => Some(
            semver::Version::parse(v)
                .map_err(|e| {
                    SeppError::Config(format!(
                        "pkg: Version {v:?} in {spec:?} ist keine semver-Version ({e})"
                    ))
                })?
                .to_string(),
        ),
        None => None,
    };
    Ok(PkgSpec {
        name: name.to_string(),
        version,
    })
}

/// Liest `[[registries]]` aus dem Text einer `settings.toml`; andere Abschnitte werden ignoriert.
pub fn parse_registries(text: &str) -> Result<Vec<RegistryConfig>> {
    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(default)]
        registries: Vec<RegistryConfig>,
    }
    let w: Wrapper = toml::from_str(text)
        .map_err(|e| SeppError::Config(format!("settings [[registries]]: {e}")))?;
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for r in &w.registries {
        validate_name(&r.name)
            .map_err(|e| SeppError::Config(format!("pkg: {} ([[registries]].name)", plain(&e))))?;
        check_url_scheme(&r.url).map_err(|e| {
            SeppError::Config(format!("pkg: {} ([[registries]] {})", plain(&e), r.name))
        })?;
        if r.url.contains(['?', '#']) {
            return Err(SeppError::Config(format!(
                "pkg: [[registries]] {}: url darf keine Query oder Fragment enthalten",
                r.name
            )));
        }
        decode_pubkey(&r.key).map_err(|e| {
            SeppError::Config(format!(
                "pkg: {} ([[registries]] {}: key)",
                plain(&e),
                r.name
            ))
        })?;
        if !seen.insert(r.name.as_str()) {
            return Err(SeppError::Config(format!(
                "pkg: doppelte Registry '{}' in settings.toml",
                r.name
            )));
        }
    }
    Ok(w.registries)
}

/// Liest `[[registries]]` aus einer `settings.toml`; eine fehlende Datei ist eine leere Liste.
pub fn load_registries(path: &Path) -> Result<Vec<RegistryConfig>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_registries(&text)
            .map_err(|e| SeppError::Config(format!("pkg: {}: {}", path.display(), plain(&e)))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(SeppError::Config(format!("pkg: {}: {e}", path.display()))),
    }
}

/// Holt Signatur und Index einer Registry und prüft beides gegen den gepinnten Schlüssel.
pub fn fetch_index(fetcher: &dyn Fetcher, cfg: &RegistryConfig) -> Result<Index> {
    check_url_scheme(&cfg.url)?;
    let sig_url = sig_url(&cfg.url);
    let sig = fetcher.fetch(&sig_url, MAX_SIG_BYTES).map_err(|e| {
        SeppError::Config(format!(
            "pkg: Registry »{}«: {sig_url}: {}",
            cfg.name,
            plain(&e)
        ))
    })?;
    let sig_text = String::from_utf8(sig).map_err(|_| {
        SeppError::Config(format!(
            "pkg: Registry »{}«: {sig_url} ist kein Text",
            cfg.name
        ))
    })?;
    let bytes = fetcher.fetch(&cfg.url, MAX_INDEX_BYTES).map_err(|e| {
        SeppError::Config(format!(
            "pkg: Registry »{}«: {}: {}",
            cfg.name,
            cfg.url,
            plain(&e)
        ))
    })?;
    verify_index(&bytes, &sig_text, &cfg.key)
        .map_err(|e| SeppError::Config(format!("pkg: Registry »{}«: {}", cfg.name, plain(&e))))
}

/// Schreibt gedeckelt in eine Datei und hasht dabei.
struct HashingWriter {
    inner: BufWriter<std::fs::File>,
    hasher: Hasher,
    written: u64,
    max: u64,
}

impl Write for HashingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written + buf.len() as u64 > self.max {
            return Err(std::io::Error::other(format!(
                "mehr als {} Bytes — der Index nennt eine kleinere Datei",
                self.max
            )));
        }
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Lädt das Paket eines Eintrags nach `<dest_dir>/<name>-<version>.seppkg` (Verzeichnis 0700),
/// gedeckelt auf `size` und gehasht beim Schreiben. Stimmen Größe oder SHA-256 nicht mit dem
/// Index überein, ist die Datei weg und das Ergebnis ein Fehler. Liefert den Pfad.
pub fn download_package(
    fetcher: &dyn Fetcher,
    index_url: &str,
    entry: &IndexEntry,
    dest_dir: &Path,
) -> Result<PathBuf> {
    validate_name(&entry.name)?;
    let version = semver::Version::parse(&entry.version).map_err(|e| {
        SeppError::Config(format!(
            "pkg: Index: {} hat keine semver-Version ({e})",
            entry.name
        ))
    })?;
    if entry.size == 0 || entry.size > MAX_TOTAL_BYTES {
        return Err(SeppError::Config(format!(
            "pkg: Index: {} {version}: size {} liegt außerhalb von 1..={MAX_TOTAL_BYTES}",
            entry.name, entry.size
        )));
    }
    let url = join_url(index_url, &entry.url)?;
    if let Some(parent) = dest_dir.parent() {
        ensure_private_dir(parent)?;
    }
    ensure_private_dir(dest_dir)?;
    let path = dest_dir.join(format!("{}-{version}.{EXTENSION}", entry.name));
    // Rest eines abgebrochenen Laufs.
    let _ = std::fs::remove_file(&path);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", path.display())))?;
    let result = (|| -> Result<()> {
        let mut w = HashingWriter {
            inner: BufWriter::new(file),
            hasher: Hasher::new(),
            written: 0,
            max: entry.size,
        };
        fetcher
            .fetch_to_writer(&url, entry.size, &mut w)
            .map_err(|e| SeppError::Config(format!("pkg: {url}: {}", plain(&e))))?;
        w.flush()
            .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", path.display())))?;
        let written = w.written;
        let hash = w.hasher.finish_hex();
        if written != entry.size {
            return Err(SeppError::Config(format!(
                "pkg: {url}: {written} Bytes geladen, der Index nennt {} — Index und Datei \
                 widersprechen sich",
                entry.size
            )));
        }
        if !hash.eq_ignore_ascii_case(entry.sha256.trim()) {
            return Err(SeppError::Config(format!(
                "pkg: {url}: SHA-256 stimmt nicht mit dem Index überein — Index und Datei \
                 widersprechen sich"
            )));
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    Ok(path)
}

/// Baut aus allen `*.seppkg` in `dir` einen signierten Index. Jedes Paket wird wie beim
/// Installieren geprüft (Signatur, Manifest) — ein kaputtes Paket ist ein Fehler, kein stilles
/// Überspringen. `url` je Eintrag ist der Dateiname, mit `base_url` absolut. Reproduzierbar bis
/// auf `generated_at`.
pub fn build_index(
    dir: &Path,
    key: &SigningKey,
    name: &str,
    base_url: Option<&str>,
) -> Result<IndexBuild> {
    validate_name(name)
        .map_err(|e| SeppError::Config(format!("pkg: {} (Index-Name)", plain(&e))))?;
    let base = match base_url {
        Some(b) => {
            check_url_scheme(b)?;
            if b.contains(['?', '#']) {
                return Err(SeppError::Config(format!(
                    "pkg: --base-url {b:?} darf keine Query oder Fragment enthalten"
                )));
            }
            Some(if b.ends_with('/') {
                b.to_string()
            } else {
                format!("{b}/")
            })
        }
        None => None,
    };
    let rd = std::fs::read_dir(dir)
        .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", dir.display())))?;
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|e| SeppError::Config(format!("pkg: {}: {e}", dir.display())))?;
        let path = entry.path();
        let is_pkg = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(&format!(".{EXTENSION}")));
        if is_pkg {
            files.push(path);
        }
    }
    files.sort();

    struct Row {
        name: String,
        version: semver::Version,
        description: Option<String>,
        publisher: String,
        publisher_key: String,
        url: String,
        sha256: String,
        size: u64,
    }
    let mut warnings = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    for path in &files {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| SeppError::Config(format!("pkg: {}: kein UTF-8", path.display())))?;
        let meta = std::fs::symlink_metadata(path)
            .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", path.display())))?;
        if meta.file_type().is_symlink() {
            warnings.push(format!("{file_name}: Symlink, übersprungen"));
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        validate_rel_url(file_name).map_err(|e| {
            SeppError::Config(format!("pkg: {} (Dateiname {file_name})", plain(&e)))
        })?;
        let signed = PkgArchive::open(path)?
            .read_signed_manifest()
            .map_err(|e| SeppError::Config(format!("pkg: {} ({file_name})", plain(&e))))?;
        let m = &signed.manifest;
        let unknown = m.unknown_keys();
        if !unknown.is_empty() {
            warnings.push(format!(
                "{file_name}: unbekannte Felder im Manifest, ohne Wirkung: {}",
                unknown.join(", ")
            ));
        }
        let version = semver::Version::parse(&m.version)
            .map_err(|e| SeppError::Config(format!("pkg: {file_name}: version: {e}")))?;
        rows.push(Row {
            name: m.name.clone(),
            version,
            description: m.description.clone(),
            publisher: m.publisher.name.clone(),
            publisher_key: m.publisher.key.trim().to_string(),
            url: match &base {
                Some(b) => format!("{b}{file_name}"),
                None => file_name.to_string(),
            },
            sha256: hash_file(path)?,
            size: meta.len(),
        });
    }
    if rows.is_empty() {
        return Err(SeppError::Config(format!(
            "pkg: {} enthält keine .{EXTENSION}-Dateien",
            dir.display()
        )));
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
    for pair in rows.windows(2) {
        if pair[0].name == pair[1].name && pair[0].version == pair[1].version {
            return Err(SeppError::Config(format!(
                "pkg: {} {} kommt in zwei Dateien vor",
                pair[0].name, pair[0].version
            )));
        }
    }

    let mut doc = toml_edit::DocumentMut::new();
    doc["format"] = toml_edit::value(i64::from(INDEX_FORMAT));
    doc["name"] = toml_edit::value(name);
    doc["generated_at"] = toml_edit::value(i64::try_from(now()).unwrap_or(0));
    let mut aot = toml_edit::ArrayOfTables::new();
    let mut entries = Vec::new();
    for r in &rows {
        let mut t = toml_edit::Table::new();
        t["name"] = toml_edit::value(r.name.as_str());
        t["version"] = toml_edit::value(r.version.to_string());
        if let Some(d) = &r.description {
            t["description"] = toml_edit::value(d.as_str());
        }
        t["publisher"] = toml_edit::value(r.publisher.as_str());
        t["publisher_key"] = toml_edit::value(r.publisher_key.as_str());
        t["url"] = toml_edit::value(r.url.as_str());
        t["sha256"] = toml_edit::value(r.sha256.as_str());
        let size = i64::try_from(r.size)
            .map_err(|_| SeppError::Config(format!("pkg: {}: Datei zu groß", r.name)))?;
        t["size"] = toml_edit::value(size);
        aot.push(t);
        entries.push((r.name.clone(), r.version.to_string()));
    }
    doc["packages"] = toml_edit::Item::ArrayOfTables(aot);
    let index_text = doc.to_string();
    // Selbstprüfung wie der Leser — der Betreiber soll seine Fehler sehen, nicht der Nutzer.
    Index::parse(&index_text)?.validate()?;
    let signature_text = format!("{}\n", encode_signature(&key.sign(index_text.as_bytes())));
    Ok(IndexBuild {
        index_text,
        signature_text,
        entries,
        fingerprint: key.fingerprint(),
        public_key: encode_pubkey(&key.public_key()),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::testutil::packed;
    use crate::crypto::sha256_hex;

    const KEY0: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const H0: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn entry(name: &str, version: &str, extra: &str) -> String {
        format!(
            "[[packages]]\nname = \"{name}\"\nversion = \"{version}\"\npublisher = \"acme\"\n\
             publisher_key = \"{KEY0}\"\nurl = \"{name}-{version}.seppkg\"\nsha256 = \"{H0}\"\n\
             size = 10\n{extra}"
        )
    }

    fn index_text(entries: &[String]) -> String {
        format!("format = 1\nname = \"test\"\n{}", entries.join(""))
    }

    /// Ein Fetcher aus einer Tabelle — deckelt bewusst **nicht**, damit der Deckel des Writers
    /// getestet wird.
    struct MapFetcher(BTreeMap<String, Vec<u8>>);

    impl Fetcher for MapFetcher {
        fn fetch_to_writer(&self, url: &str, _max: u64, out: &mut dyn Write) -> Result<u64> {
            let body = self
                .0
                .get(url)
                .ok_or_else(|| SeppError::Config(format!("HTTP 404 für {url}")))?;
            out.write_all(body)
                .map_err(|e| SeppError::Config(e.to_string()))?;
            Ok(body.len() as u64)
        }
    }

    #[test]
    fn index_parses_validates_and_reports_unknown_fields() {
        // Ein unbekanntes Feld oben (vor den Tabellen) und eines im Eintrag.
        let text = index_text(&[entry("demo", "1.0.0", "farbe = \"blau\"\n")])
            .replace("name = \"test\"\n", "name = \"test\"\nextra = 1\n");
        let i = Index::parse(&text).unwrap();
        i.validate().unwrap();
        assert_eq!(i.format, 1);
        assert_eq!(i.packages.len(), 1);
        assert_eq!(
            i.unknown_keys(),
            vec!["extra", "packages[demo@1.0.0].farbe"]
        );
        assert!(Index::parse("name = 1").is_err());
    }

    #[test]
    fn validate_rejects_each_broken_field() {
        let ok = index_text(&[entry("demo", "1.0.0", "")]);
        Index::parse(&ok).unwrap().validate().unwrap();
        let cases: Vec<(String, &str)> = vec![
            (ok.replace("format = 1", "format = 2"), "neueres sepp"),
            (
                ok.replace("name = \"test\"", "name = \"Test\""),
                "Index: name",
            ),
            (
                ok.replace("name = \"demo\"", "name = \"Demo\""),
                "packages.name",
            ),
            (
                ok.replace("version = \"1.0.0\"", "version = \"eins\""),
                "semver",
            ),
            (
                ok.replace("publisher = \"acme\"", "publisher = \"A\""),
                "publisher",
            ),
            (ok.replace(KEY0, "AAAA"), "publisher_key"),
            (ok.replace(H0, "abc"), "sha256"),
            (ok.replace("size = 10", "size = 0"), "size"),
            (
                ok.replace("size = 10", &format!("size = {}", MAX_TOTAL_BYTES + 1)),
                "size",
            ),
            (ok.replace("demo-1.0.0.seppkg", "../x.seppkg"), "url"),
            (ok.replace("demo-1.0.0.seppkg", "x.seppkg?y"), "url"),
            (ok.replace("demo-1.0.0.seppkg", "/x.seppkg"), "url"),
            (
                ok.replace("demo-1.0.0.seppkg", "http://evil.example/x"),
                "url",
            ),
            (
                index_text(&[entry("demo", "1.0.0", ""), entry("demo", "1.0.0", "")]),
                "doppelt",
            ),
        ];
        for (text, needle) in cases {
            let e = Index::parse(&text)
                .and_then(|i| i.validate())
                .unwrap_err()
                .to_string();
            assert!(e.contains(needle), "{needle}: {e}\n{text}");
        }
        // Absolut nach der Schema-Regel ist erlaubt.
        let abs = ok.replace("demo-1.0.0.seppkg", "https://pkg.example/demo.seppkg");
        Index::parse(&abs).unwrap().validate().unwrap();
    }

    #[test]
    fn verify_index_rejects_tampered_wrong_key_and_oversize() {
        let (key, _) = SigningKey::generate().unwrap();
        let (other, _) = SigningKey::generate().unwrap();
        let text = index_text(&[entry("demo", "1.0.0", "")]);
        let sig = format!("{}\n", encode_signature(&key.sign(text.as_bytes())));
        let pk = encode_pubkey(&key.public_key());
        let i = verify_index(text.as_bytes(), &sig, &pk).unwrap();
        assert_eq!(i.packages[0].name, "demo");

        let tampered = text.replace("size = 10", "size = 11");
        let e = verify_index(tampered.as_bytes(), &sig, &pk)
            .unwrap_err()
            .to_string();
        assert!(e.contains("Signatur"), "{e}");
        let e = verify_index(text.as_bytes(), &sig, &encode_pubkey(&other.public_key()))
            .unwrap_err()
            .to_string();
        assert!(e.contains("Signatur"), "{e}");
        assert!(verify_index(text.as_bytes(), "kein base64", &pk).is_err());
        assert!(verify_index(text.as_bytes(), &sig, "AAAA").is_err());
        let big = vec![b' '; MAX_INDEX_BYTES as usize + 1];
        let e = verify_index(&big, &sig, &pk).unwrap_err().to_string();
        assert!(e.contains("größer"), "{e}");
    }

    #[test]
    fn resolve_picks_highest_or_exact_and_reports_missing() {
        let text = index_text(&[
            entry("demo", "1.0.0", ""),
            entry("demo", "1.10.0", ""),
            entry("demo", "1.2.0", ""),
            entry("other", "0.1.0", ""),
        ]);
        let i = Index::parse(&text).unwrap();
        i.validate().unwrap();
        assert_eq!(i.resolve("demo", None).unwrap().version, "1.10.0");
        assert_eq!(i.resolve("demo", Some("1.2.0")).unwrap().version, "1.2.0");
        let e = i.resolve("demo", Some("9.9.9")).unwrap_err().to_string();
        assert!(e.contains("9.9.9") && e.contains("1.10.0"), "{e}");
        let e = i.resolve("fremd", None).unwrap_err().to_string();
        assert!(e.contains("nicht im Index"), "{e}");
        assert!(i.resolve("demo", Some("eins")).is_err());
    }

    #[test]
    fn matching_filters_by_text_and_keeps_latest_per_name() {
        let text = index_text(&[
            entry("demo", "1.0.0", "description = \"Rechnungen prüfen\"\n"),
            entry("demo", "2.0.0", "description = \"Rechnungen prüfen\"\n"),
            entry("belege", "0.5.0", "description = \"Belege sortieren\"\n"),
        ]);
        let i = Index::parse(&text).unwrap();
        let all = i.matching(None);
        assert_eq!(
            all.iter()
                .map(|e| (e.name.as_str(), e.version.as_str()))
                .collect::<Vec<_>>(),
            vec![("belege", "0.5.0"), ("demo", "2.0.0")]
        );
        let hits = i.matching(Some("RECHNUNG"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "demo");
        assert_eq!(i.matching(Some("bel")).len(), 1);
        assert!(i.matching(Some("nix")).is_empty());
        assert_eq!(i.matching(Some("")).len(), 2);
    }

    #[test]
    fn url_scheme_rule_join_and_sig_url() {
        for ok in [
            "https://pkg.example/index.toml",
            "http://localhost:8000/index.toml",
            "http://127.0.0.1/index.toml",
            "http://[::1]:9/index.toml",
        ] {
            check_url_scheme(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "http://evil.example/index.toml",
            "http://localhost.evil.example/",
            "ftp://pkg.example/x",
            "HTTPS://pkg.example/x",
            "https://",
            "pkg.example/index.toml",
            // Zeichen, die im Terminal etwas anderes tun, als sie aussehen: Die URL steht in
            // jeder Fehlermeldung und in der Zustimmungszeile „Quelle".
            "https://pkg.example/\u{1b}[2Kx",
            "https://pkg.example/a b",
            "https://pkg.\nexample/x",
            "https://pkg.example/\u{7f}",
            // Rechts-nach-links: erschiene als ein ganz anderer Host. `url_host` allein ließe
            // das durch — `is_control()` kennt nur Steuer-, nicht Formatzeichen.
            "https://\u{202E}gro.esiob/x",
            "https://pkg.exa\u{200B}mple/x",
        ] {
            assert!(check_url_scheme(bad).is_err(), "{bad}");
        }

        let base = "https://h.example/r/index.toml";
        assert_eq!(
            join_url(base, "a.seppkg").unwrap(),
            "https://h.example/r/a.seppkg"
        );
        assert_eq!(
            join_url(base, "sub/a.seppkg").unwrap(),
            "https://h.example/r/sub/a.seppkg"
        );
        assert_eq!(
            join_url("https://h.example", "a.seppkg").unwrap(),
            "https://h.example/a.seppkg"
        );
        assert_eq!(
            join_url(base, "https://cdn.example/a.seppkg").unwrap(),
            "https://cdn.example/a.seppkg"
        );
        for bad in [
            "../x",
            "/x",
            "x?y",
            "x#y",
            "a\\b",
            "",
            "a//b",
            "./x",
            "http://evil.example/x",
        ] {
            assert!(join_url(base, bad).is_err(), "{bad}");
        }
        assert!(join_url("http://evil.example/index.toml", "a.seppkg").is_err());

        assert_eq!(sig_url(base), "https://h.example/r/index.sig");
        assert_eq!(
            sig_url("https://h.example/r/idx"),
            "https://h.example/r/idx.sig"
        );
    }

    #[test]
    fn parse_spec_forms() {
        assert_eq!(
            parse_spec("demo").unwrap(),
            PkgSpec {
                name: "demo".into(),
                version: None
            }
        );
        assert_eq!(
            parse_spec("demo@1.2.3").unwrap().version.as_deref(),
            Some("1.2.3")
        );
        for bad in [
            "Demo",
            "demo@eins",
            "demo@",
            "@1.0.0",
            "demo@1.0.0@x",
            "a-b",
        ] {
            assert!(parse_spec(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn registries_parse_missing_duplicate_badkey_http_and_query() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_registries(&tmp.path().join("fehlt.toml"))
            .unwrap()
            .is_empty());
        let good = format!(
            "[[mcp.servers]]\nname = \"git\"\ntransport = \"stdio\"\n\n[policy]\nmode = \"ask\"\n\n\
             [[registries]]\nname = \"kionova\"\nurl = \"https://pkg.example/index.toml\"\nkey = \"{KEY0}\"\n\n\
             [[registries]]\nname = \"lokal\"\nurl = \"http://127.0.0.1:8000/index.toml\"\nkey = \"{KEY0}\"\n"
        );
        let regs = parse_registries(&good).unwrap();
        assert_eq!(regs.len(), 2);
        assert_eq!(regs[0].name, "kionova");
        let p = tmp.path().join("settings.toml");
        std::fs::write(&p, &good).unwrap();
        assert_eq!(load_registries(&p).unwrap(), regs);
        assert!(parse_registries("# nur Kommentar\n").unwrap().is_empty());

        for (bad, needle) in [
            (
                good.replace("name = \"lokal\"", "name = \"kionova\""),
                "doppelte",
            ),
            (good.replace(KEY0, "AAAA"), "key"),
            (
                good.replace("http://127.0.0.1:8000", "http://evil.example"),
                "https",
            ),
            (
                good.replace("index.toml\"\nkey", "index.toml?x=1\"\nkey"),
                "Query",
            ),
            (
                good.replace("name = \"lokal\"", "name = \"Lokal\""),
                "unzulässig",
            ),
        ] {
            let e = parse_registries(&bad).unwrap_err().to_string();
            assert!(e.contains(needle), "{needle}: {e}");
        }
        assert!(parse_registries("[[registries]]\nname = \"x\"\n").is_err());
    }

    #[test]
    fn build_index_is_reproducible_sorted_and_signed() {
        let tmp = tempfile::tempdir().unwrap();
        let (p1, _) = packed(tmp.path(), "demo", "1.0.0", "");
        let (p2, _) = packed(tmp.path(), "demo", "1.10.0", "");
        let (p3, _) = packed(tmp.path(), "belege", "0.1.0", "");
        let (key, _) = SigningKey::generate().unwrap();

        let b1 = build_index(tmp.path(), &key, "test", None).unwrap();
        let b2 = build_index(tmp.path(), &key, "test", None).unwrap();
        let strip = |s: &str| {
            s.lines()
                .filter(|l| !l.starts_with("generated_at"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(strip(&b1.index_text), strip(&b2.index_text));
        assert_eq!(
            b1.entries,
            vec![
                ("belege".to_string(), "0.1.0".to_string()),
                ("demo".to_string(), "1.0.0".to_string()),
                ("demo".to_string(), "1.10.0".to_string()),
            ]
        );
        assert!(b1.warnings.is_empty(), "{:?}", b1.warnings);
        assert_eq!(b1.fingerprint, key.fingerprint());
        assert_eq!(b1.public_key, encode_pubkey(&key.public_key()));

        let index =
            verify_index(b1.index_text.as_bytes(), &b1.signature_text, &b1.public_key).unwrap();
        assert_eq!(index.name, "test");
        assert_eq!(index.format, 1);
        for (path, name, version) in [
            (&p1, "demo", "1.0.0"),
            (&p2, "demo", "1.10.0"),
            (&p3, "belege", "0.1.0"),
        ] {
            let e = index.resolve(name, Some(version)).unwrap();
            let bytes = std::fs::read(path).unwrap();
            assert_eq!(e.sha256, sha256_hex(&bytes));
            assert_eq!(e.size, bytes.len() as u64);
            assert_eq!(e.url, format!("{name}-{version}.seppkg"));
            assert_eq!(e.publisher, "acme");
            assert_eq!(e.description.as_deref(), Some("Demo"));
        }

        let abs = build_index(tmp.path(), &key, "test", Some("https://pkg.example/r")).unwrap();
        assert!(abs
            .index_text
            .contains("url = \"https://pkg.example/r/demo-1.0.0.seppkg\""));
        assert!(build_index(tmp.path(), &key, "test", Some("http://evil.example/")).is_err());
        assert!(build_index(tmp.path(), &key, "Test", None).is_err());

        // Kaputtes Paket → Fehler, kein stilles Überspringen.
        std::fs::write(
            tmp.path().join("kaputt-0.0.1.seppkg"),
            b"\x28\xB5\x2F\xFDxx",
        )
        .unwrap();
        let e = build_index(tmp.path(), &key, "test", None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("kaputt-0.0.1.seppkg"), "{e}");
        std::fs::remove_file(tmp.path().join("kaputt-0.0.1.seppkg")).unwrap();

        let empty = tempfile::tempdir().unwrap();
        assert!(build_index(empty.path(), &key, "test", None).is_err());
    }

    #[test]
    fn fetch_index_with_fake_fetcher() {
        let (key, _) = SigningKey::generate().unwrap();
        let text = index_text(&[entry("demo", "1.0.0", "")]);
        let sig = format!("{}\n", encode_signature(&key.sign(text.as_bytes())));
        let cfg = RegistryConfig {
            name: "test".into(),
            url: "https://pkg.example/r/index.toml".into(),
            key: encode_pubkey(&key.public_key()),
        };
        let mut map = BTreeMap::new();
        map.insert(cfg.url.clone(), text.clone().into_bytes());
        map.insert(
            "https://pkg.example/r/index.sig".to_string(),
            sig.clone().into_bytes(),
        );
        let i = fetch_index(&MapFetcher(map.clone()), &cfg).unwrap();
        assert_eq!(i.packages.len(), 1);

        let mut wrong = map.clone();
        wrong.insert("https://pkg.example/r/index.sig".to_string(), {
            let mut w = sig.clone().into_bytes();
            w[0] = if w[0] == b'A' { b'B' } else { b'A' };
            w
        });
        let e = fetch_index(&MapFetcher(wrong), &cfg)
            .unwrap_err()
            .to_string();
        assert!(e.contains("»test«"), "{e}");
        let mut missing = map;
        missing.remove("https://pkg.example/r/index.sig");
        let e = fetch_index(&MapFetcher(missing), &cfg)
            .unwrap_err()
            .to_string();
        assert!(e.contains("404") && e.contains("index.sig"), "{e}");
    }

    #[test]
    fn download_package_checks_hash_size_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let body = b"\x28\xB5\x2F\xFD paketinhalt".to_vec();
        let mk = |sha: &str, size: u64| IndexEntry {
            name: "demo".into(),
            version: "1.0.0".into(),
            description: None,
            publisher: "acme".into(),
            publisher_key: KEY0.into(),
            url: "demo-1.0.0.seppkg".into(),
            sha256: sha.into(),
            size,
            unknown: BTreeMap::new(),
        };
        let index_url = "https://pkg.example/r/index.toml";
        let mut map = BTreeMap::new();
        map.insert(
            "https://pkg.example/r/demo-1.0.0.seppkg".to_string(),
            body.clone(),
        );
        let f = MapFetcher(map);
        let dest = tmp.path().join("state").join("pkg").join("downloads");

        let ok = mk(&sha256_hex(&body), body.len() as u64);
        let path = download_package(&f, index_url, &ok, &dest).unwrap();
        assert_eq!(path, dest.join("demo-1.0.0.seppkg"));
        assert_eq!(std::fs::read(&path).unwrap(), body);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for d in [&dest, &tmp.path().join("state").join("pkg")] {
                assert_eq!(
                    std::fs::metadata(d).unwrap().permissions().mode() & 0o777,
                    0o700,
                    "{}",
                    d.display()
                );
            }
        }
        // Ein zweiter Lauf ersetzt die Datei.
        download_package(&f, index_url, &ok, &dest).unwrap();

        let e = download_package(&f, index_url, &mk(H0, body.len() as u64), &dest)
            .unwrap_err()
            .to_string();
        assert!(e.contains("SHA-256"), "{e}");
        assert!(!path.exists(), "Datei nach Hash-Fehler weg");

        let e = download_package(
            &f,
            index_url,
            &mk(&sha256_hex(&body), body.len() as u64 - 1),
            &dest,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("Bytes"), "{e}");
        assert!(!path.exists());

        let e = download_package(
            &f,
            index_url,
            &mk(&sha256_hex(&body), body.len() as u64 + 1),
            &dest,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("Bytes"), "{e}");
        assert!(!path.exists());

        let mut gone = mk(&sha256_hex(&body), body.len() as u64);
        gone.url = "fehlt.seppkg".into();
        let e = download_package(&f, index_url, &gone, &dest)
            .unwrap_err()
            .to_string();
        assert!(e.contains("404"), "{e}");
        assert!(
            std::fs::read_dir(&dest).unwrap().next().is_none(),
            "downloads leer"
        );
    }
}
