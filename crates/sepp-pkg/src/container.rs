//! Der Container: zstd-komprimiertes tar, lesen und schreiben — mit den Prüfungen, die zum
//! Entpacken fremder Archive gehören.
//!
//! **Nie `Archive::unpack`.** Ein tar kann absolute Pfade, `..`, Symlinks auf `~/.ssh` und
//! Header mit Gigabyte-Größen enthalten. Jeder Eintrag wird deshalb von Hand geprüft — Pfad,
//! Typ, Größe, Zahl — und mit `create_new` in ein frisches Verzeichnis geschrieben, während sein
//! SHA-256 mitläuft. Erst wenn die Signatur über das Manifest steht, wird überhaupt gelesen.
//!
//! Beim Packen ist das Archiv reproduzierbar (Modus 0644, mtime 0, uid/gid 0, sortierte
//! Einträge): zweimal packen ergibt dieselben Bytes, und ein Diff zweier Pakete ist ein Diff
//! ihrer Inhalte. Der Codec ist in [`compress`]/[`decompress`] gekapselt — sollte zstd im
//! statischen musl-Build je Ärger machen, ist der Wechsel auf einen reinen Rust-Codec eine
//! lokale Änderung, solange Format 1 nicht draußen ist.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use sepp_core::{Result, SeppError};
use sepp_policy::Manifest;

use crate::crypto::{self, Hasher, SigningKey};
use crate::manifest::PkgManifest;
use crate::{
    validate_rel_path, CONTENT_DIRS, CONTENT_FILES, MAX_ENTRIES, MAX_FILE_BYTES,
    MAX_MANIFEST_BYTES, MAX_TOTAL_BYTES,
};

/// Die vier Magic-Bytes eines zstd-Frames.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const MANIFEST: &str = "manifest.toml";
const SIGNATURE: &str = "manifest.sig";
/// Kompressionsstufe beim Packen — Plugins sind schon klein, Skills sind Text.
const LEVEL: i32 = 10;

/// Ergebnis von [`pack_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackReport {
    pub out: PathBuf,
    pub files: usize,
    pub bytes: u64,
    pub fingerprint: String,
    /// Hinweise, die kein Fehler sind (unbekannte Manifest-Felder).
    pub warnings: Vec<String>,
}

/// Ergebnis von [`PkgArchive::extract_verified`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractReport {
    pub files: usize,
    pub bytes: u64,
}

/// Das geprüfte Manifest eines Archivs: Text, Struktur, Signatur, Fingerprint des Herausgebers.
#[derive(Debug, Clone)]
pub struct Signed {
    pub manifest_text: String,
    pub manifest: PkgManifest,
    pub signature: Vec<u8>,
    pub fingerprint: String,
}

/// Komprimiert `reader` nach `writer`.
pub fn compress<R: Read, W: Write>(mut reader: R, writer: W) -> std::io::Result<W> {
    let mut enc = zstd::stream::write::Encoder::new(writer, LEVEL)?;
    std::io::copy(&mut reader, &mut enc)?;
    enc.finish()
}

/// Dekomprimiert `reader` — gedeckelt auf [`MAX_TOTAL_BYTES`] plus Headerreserve, damit eine
/// kleine Datei nicht zu Gigabytes wird.
pub fn decompress<'a, R: Read + 'a>(reader: R) -> std::io::Result<impl Read + 'a> {
    // tar-Header: 512 Byte je Eintrag plus Auffüllung — großzügig reserviert.
    let cap = MAX_TOTAL_BYTES + (MAX_ENTRIES as u64 + 2) * 1024 + 1;
    Ok(zstd::stream::read::Decoder::new(reader)?.take(cap))
}

/// Baut ein Paket aus einem Verzeichnis: liest `manifest.toml` (ohne `[files]`), berechnet die
/// Hashes, trägt Public Key und `[files]` kommentarerhaltend ein, prüft, signiert, packt.
///
/// `out` = Zieldatei; Konvention `<name>-<version>.seppkg`.
pub fn pack_dir(dir: &Path, key: &SigningKey, out: &Path) -> Result<PackReport> {
    let manifest_path = dir.join(MANIFEST);
    let source = std::fs::read_to_string(&manifest_path)
        .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", manifest_path.display())))?;
    let mut doc: toml_edit::DocumentMut = source
        .parse()
        .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", manifest_path.display())))?;
    if doc.get("files").is_some() {
        return Err(SeppError::Config(
            "pkg: manifest.toml enthält schon [files] — die Liste schreibt `pack` selbst; \
             Abschnitt entfernen"
                .into(),
        ));
    }

    // Dateien einsammeln: sortiert, nur reguläre Dateien, nichts außerhalb der Allowlist.
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    collect_files(dir, dir, &mut files)?;
    files.sort();
    if files.is_empty() {
        return Err(SeppError::Config(format!(
            "pkg: {} enthält nichts, was in ein Paket gehört ({}/)",
            dir.display(),
            CONTENT_DIRS.join("/, ")
        )));
    }
    if files.len() > MAX_ENTRIES {
        return Err(SeppError::Config(format!(
            "pkg: mehr als {MAX_ENTRIES} Dateien"
        )));
    }

    // Hashes und Größen.
    let mut total: u64 = 0;
    let mut files_table = toml_edit::Table::new();
    for (rel, abs) in &files {
        let meta = std::fs::metadata(abs)
            .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", abs.display())))?;
        if meta.len() > MAX_FILE_BYTES {
            return Err(SeppError::Config(format!(
                "pkg: {rel} ist größer als {MAX_FILE_BYTES} Bytes"
            )));
        }
        total += meta.len();
        let hash = hash_file(abs)?;
        files_table[rel.as_str()] = toml_edit::value(hash);
    }
    if total > MAX_TOTAL_BYTES {
        return Err(SeppError::Config(format!(
            "pkg: Paket ist größer als {MAX_TOTAL_BYTES} Bytes"
        )));
    }

    // Public Key und [files] eintragen.
    let pubkey_b64 = crypto::encode_pubkey(&key.public_key());
    let publisher = doc
        .get_mut("publisher")
        .and_then(|p| p.as_table_mut())
        .ok_or_else(|| SeppError::Config("pkg: manifest.toml braucht [publisher]".into()))?;
    if let Some(existing) = publisher.get("key").and_then(|k| k.as_str()) {
        if existing != pubkey_b64 {
            return Err(SeppError::Config(
                "pkg: [publisher].key im Manifest passt nicht zum Signierschlüssel — Zeile \
                 entfernen, `pack` trägt den richtigen ein"
                    .into(),
            ));
        }
    }
    publisher["key"] = toml_edit::value(pubkey_b64);
    doc["files"] = toml_edit::Item::Table(files_table);
    let manifest_text = doc.to_string();

    // Prüfen wie der Installer — der Herausgeber soll seine Fehler sehen, nicht der Nutzer.
    let manifest = PkgManifest::parse(&manifest_text)?;
    manifest.validate()?;
    check_plugin_pairs(dir, &manifest)?;
    let mut warnings = Vec::new();
    let unknown = manifest.unknown_keys();
    if !unknown.is_empty() {
        warnings.push(format!(
            "unbekannte Felder im Manifest, ohne Wirkung: {}",
            unknown.join(", ")
        ));
    }

    let signature = key.sign(manifest_text.as_bytes());
    let sig_text = format!("{}\n", crypto::encode_signature(&signature));

    // Schreiben: tar in zstd in Datei, nie über ein vorhandenes Paket hinweg.
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)
        .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", out.display())))?;
    let result = (|| -> std::io::Result<()> {
        let enc = zstd::stream::write::Encoder::new(BufWriter::new(file), LEVEL)?;
        let mut tar = tar::Builder::new(enc);
        append_bytes(&mut tar, MANIFEST, manifest_text.as_bytes())?;
        append_bytes(&mut tar, SIGNATURE, sig_text.as_bytes())?;
        for (rel, abs) in &files {
            let mut f = File::open(abs)?;
            let len = f.metadata()?.len();
            let mut header = tar::Header::new_ustar();
            fill_header(&mut header, len);
            tar.append_data(&mut header, rel, &mut f)?;
        }
        let enc = tar.into_inner()?;
        let mut w = enc.finish()?;
        w.flush()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(out);
        return Err(SeppError::Config(format!("pkg: {}: {e}", out.display())));
    }
    Ok(PackReport {
        out: out.to_path_buf(),
        files: files.len(),
        bytes: total,
        fingerprint: key.fingerprint(),
        warnings,
    })
}

fn fill_header(header: &mut tar::Header, len: u64) {
    header.set_size(len);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_entry_type(tar::EntryType::Regular);
}

fn append_bytes<W: Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut header = tar::Header::new_ustar();
    fill_header(&mut header, bytes.len() as u64);
    tar.append_data(&mut header, name, bytes)
}

/// Sammelt rekursiv alle Dateien unter `dir`, relativ zu `root`. Symlinks sind ein Fehler,
/// alles außerhalb der Allowlist ebenfalls — ein Herausgeber soll wissen, was er ausliefert.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let rd = std::fs::read_dir(dir)
        .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", dir.display())))?;
    let mut entries: Vec<PathBuf> = rd
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()
        .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", dir.display())))?;
    entries.sort();
    for path in entries {
        let rel = path
            .strip_prefix(root)
            .map_err(|_| SeppError::Config(format!("pkg: {} liegt außerhalb", path.display())))?;
        let rel_str = rel
            .to_str()
            .ok_or_else(|| {
                SeppError::Config(format!("pkg: Pfad {} ist kein UTF-8", rel.display()))
            })?
            .replace('\\', "/");
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", path.display())))?;
        if meta.file_type().is_symlink() {
            return Err(SeppError::Config(format!(
                "pkg: {rel_str} ist ein Symlink — Pakete enthalten nur reguläre Dateien"
            )));
        }
        // Das Manifest selbst und die Signatur (von einem früheren Lauf) gehören nicht dazu.
        if rel_str == MANIFEST || rel_str == SIGNATURE {
            continue;
        }
        // Versteckte Dateien (.git, .DS_Store) werden übergangen, nicht gemeldet.
        if rel
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        if meta.is_dir() {
            if rel.components().count() == 1 && !CONTENT_DIRS.contains(&rel_str.as_str()) {
                return Err(SeppError::Config(format!(
                    "pkg: Verzeichnis {rel_str}/ gehört nicht in ein Paket (erlaubt: {})",
                    CONTENT_DIRS.join("/, ")
                )));
            }
            collect_files(root, &path, out)?;
        } else if meta.is_file() {
            if rel.components().count() == 1 && !CONTENT_FILES.contains(&rel_str.as_str()) {
                return Err(SeppError::Config(format!(
                    "pkg: Datei {rel_str} gehört nicht in ein Paket (erlaubt oben: {})",
                    CONTENT_FILES.join(", ")
                )));
            }
            validate_rel_path(&rel_str)?;
            out.push((rel_str, path));
        }
    }
    Ok(())
}

/// Jedes `plugins/<n>.wasm` braucht ein `plugins/<n>.toml`, dessen `name` gleich `<n>` ist —
/// sonst suchte Sepp Guard die Rechte unter einem anderen Namen als der Loader.
fn check_plugin_pairs(dir: &Path, manifest: &PkgManifest) -> Result<()> {
    for stem in &manifest.inventory().plugins {
        let toml_path = dir.join("plugins").join(format!("{stem}.toml"));
        let m = Manifest::from_file(&toml_path)?;
        if m.name != *stem {
            return Err(SeppError::Config(format!(
                "pkg: plugins/{stem}.toml hat name = {:?}, erwartet {stem:?} (Dateistamm)",
                m.name
            )));
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let mut f =
        File::open(path).map_err(|e| SeppError::Config(format!("pkg: {}: {e}", path.display())))?;
    let mut h = Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finish_hex())
}

/// Ein geöffnetes Paket. `open` prüft nur die Magic-Bytes; gelesen wird erst in
/// [`read_signed_manifest`](Self::read_signed_manifest).
#[derive(Debug)]
pub struct PkgArchive {
    path: PathBuf,
}

impl PkgArchive {
    pub fn open(path: &Path) -> Result<Self> {
        let mut f = File::open(path)
            .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", path.display())))?;
        let mut magic = [0u8; 4];
        f.read_exact(&mut magic).map_err(|_| {
            SeppError::Config(format!("pkg: {} ist zu kurz für ein Paket", path.display()))
        })?;
        if magic != ZSTD_MAGIC {
            return Err(SeppError::Config(format!(
                "pkg: {} ist kein .seppkg (kein zstd-Frame)",
                path.display()
            )));
        }
        Ok(PkgArchive {
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn archive(&self) -> Result<tar::Archive<Box<dyn Read + '_>>> {
        let f = File::open(&self.path)
            .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", self.path.display())))?;
        let dec = decompress(BufReader::new(f))
            .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", self.path.display())))?;
        Ok(tar::Archive::new(Box::new(dec)))
    }

    /// Liest die ersten beiden Einträge, prüft die Signatur gegen den Schlüssel im Manifest und
    /// validiert das Manifest. Danach ist bekannt, wer das Paket herausgibt und was drin sein
    /// muss — gelesen wurde noch keine Nutzdatei.
    pub fn read_signed_manifest(&self) -> Result<Signed> {
        let mut archive = self.archive()?;
        let mut entries = archive
            .entries()
            .map_err(|e| SeppError::Config(format!("pkg: {e}")))?;
        let manifest_text = read_named(&mut entries, MANIFEST, MAX_MANIFEST_BYTES)?;
        let sig_text = read_named(&mut entries, SIGNATURE, 256)?;
        let manifest_text = String::from_utf8(manifest_text)
            .map_err(|_| SeppError::Config("pkg: manifest.toml ist kein UTF-8".into()))?;
        let sig_text = String::from_utf8(sig_text)
            .map_err(|_| SeppError::Config("pkg: manifest.sig ist kein UTF-8".into()))?;
        let manifest = PkgManifest::parse(&manifest_text)?;
        let pubkey = crypto::decode_pubkey(&manifest.publisher.key)?;
        let signature = crypto::decode_signature(&sig_text)?;
        crypto::verify(&pubkey, manifest_text.as_bytes(), &signature)?;
        manifest.validate()?;
        Ok(Signed {
            fingerprint: crypto::fingerprint(&pubkey),
            manifest_text,
            manifest,
            signature,
        })
    }

    /// Liest nur die Plugin-Manifeste (`plugins/<stamm>.toml`) aus dem Archiv, geprüft gegen
    /// `[files]` — damit die Rechteprüfung **vor** dem Entpacken laufen kann. Liefert je Stamm
    /// den TOML-Text.
    pub fn read_plugin_manifests(&self, signed: &Signed) -> Result<BTreeMap<String, String>> {
        let wanted: BTreeMap<&str, (&str, &str)> = signed
            .manifest
            .files
            .iter()
            .filter_map(|(path, hash)| {
                let stem = path.strip_prefix("plugins/")?.strip_suffix(".toml")?;
                Some((path.as_str(), (stem, hash.as_str())))
            })
            .collect();
        let mut out = BTreeMap::new();
        if wanted.is_empty() {
            return Ok(out);
        }
        let mut archive = self.archive()?;
        let entries = archive
            .entries()
            .map_err(|e| SeppError::Config(format!("pkg: {e}")))?;
        for entry in entries {
            let entry = entry.map_err(|e| SeppError::Config(format!("pkg: {e}")))?;
            let path = entry_path(&entry)?;
            let Some((stem, want)) = wanted.get(path.as_str()) else {
                continue;
            };
            if entry.header().entry_type() != tar::EntryType::Regular {
                return Err(SeppError::Config(format!("pkg: {path} ist keine Datei")));
            }
            if entry.size() > MAX_MANIFEST_BYTES {
                return Err(SeppError::Config(format!(
                    "pkg: {path} ist größer als {MAX_MANIFEST_BYTES} Bytes"
                )));
            }
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry
                .take(MAX_MANIFEST_BYTES + 1)
                .read_to_end(&mut buf)
                .map_err(|e| SeppError::Config(format!("pkg: {path}: {e}")))?;
            if crypto::sha256_hex(&buf) != *want {
                return Err(SeppError::Config(format!(
                    "pkg: {path}: SHA-256 stimmt nicht mit dem signierten Manifest überein"
                )));
            }
            let text = String::from_utf8(buf)
                .map_err(|_| SeppError::Config(format!("pkg: {path} ist kein UTF-8")))?;
            out.insert(stem.to_string(), text);
        }
        for (path, _) in wanted {
            let stem = path
                .strip_prefix("plugins/")
                .and_then(|s| s.strip_suffix(".toml"));
            if stem.is_some_and(|s| !out.contains_key(s)) {
                return Err(SeppError::Config(format!(
                    "pkg: {path} steht im Manifest, fehlt aber im Archiv"
                )));
            }
        }
        Ok(out)
    }

    /// Entpackt die Nutzdateien nach `dest` (muss leer sein oder fehlen) und prüft dabei jede
    /// gegen `[files]`. Bei jedem Fehler bleibt `dest` in unbestimmtem Zustand — der Aufrufer
    /// löscht es.
    pub fn extract_verified(&self, signed: &Signed, dest: &Path) -> Result<ExtractReport> {
        std::fs::create_dir_all(dest)
            .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", dest.display())))?;
        let expected = &signed.manifest.files;
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut total: u64 = 0;
        let mut count: usize = 0;

        let mut archive = self.archive()?;
        let entries = archive
            .entries()
            .map_err(|e| SeppError::Config(format!("pkg: {e}")))?;
        for (i, entry) in entries.enumerate() {
            let mut entry =
                entry.map_err(|e| SeppError::Config(format!("pkg: Eintrag {i}: {e}")))?;
            let path = entry_path(&entry)?;
            // Die beiden Kopf-Einträge sind schon geprüft; ein zweites Vorkommen wäre ein Angriff.
            if i < 2 {
                if (i == 0 && path != MANIFEST) || (i == 1 && path != SIGNATURE) {
                    return Err(SeppError::Config(
                        "pkg: manifest.toml und manifest.sig müssen die ersten Einträge sein"
                            .into(),
                    ));
                }
                continue;
            }
            if path == MANIFEST || path == SIGNATURE {
                return Err(SeppError::Config(format!(
                    "pkg: {path} kommt ein zweites Mal vor"
                )));
            }
            match entry.header().entry_type() {
                tar::EntryType::Directory => continue,
                tar::EntryType::Regular => {}
                other => {
                    return Err(SeppError::Config(format!(
                        "pkg: {path}: Eintragstyp {other:?} ist nicht erlaubt (nur Dateien)"
                    )))
                }
            }
            count += 1;
            if count > MAX_ENTRIES {
                return Err(SeppError::Config(format!(
                    "pkg: mehr als {MAX_ENTRIES} Einträge"
                )));
            }
            validate_rel_path(&path)?;
            let Some(want) = expected.get(&path) else {
                return Err(SeppError::Config(format!(
                    "pkg: {path} steht nicht in [files] des signierten Manifests"
                )));
            };
            if !seen.insert(path.clone()) {
                return Err(SeppError::Config(format!("pkg: {path} kommt zweimal vor")));
            }
            let size = entry.size();
            if size > MAX_FILE_BYTES {
                return Err(SeppError::Config(format!(
                    "pkg: {path}: {size} Bytes überschreiten {MAX_FILE_BYTES}"
                )));
            }
            total += size;
            if total > MAX_TOTAL_BYTES {
                return Err(SeppError::Config(format!(
                    "pkg: Paket überschreitet {MAX_TOTAL_BYTES} Bytes"
                )));
            }

            let target = dest.join(&path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", parent.display())))?;
            }
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", target.display())))?;
            let mut h = Hasher::new();
            let mut buf = [0u8; 64 * 1024];
            let mut written: u64 = 0;
            loop {
                let n = entry
                    .read(&mut buf)
                    .map_err(|e| SeppError::Config(format!("pkg: {path}: {e}")))?;
                if n == 0 {
                    break;
                }
                written += n as u64;
                if written > size {
                    return Err(SeppError::Config(format!(
                        "pkg: {path}: mehr Daten als der Header ankündigt"
                    )));
                }
                h.update(&buf[..n]);
                out.write_all(&buf[..n])
                    .map_err(|e| SeppError::Config(format!("pkg: {}: {e}", target.display())))?;
            }
            let got = h.finish_hex();
            if got != *want {
                return Err(SeppError::Config(format!(
                    "pkg: {path}: SHA-256 stimmt nicht mit dem signierten Manifest überein"
                )));
            }
        }
        let missing: Vec<&String> = expected.keys().filter(|k| !seen.contains(*k)).collect();
        if !missing.is_empty() {
            return Err(SeppError::Config(format!(
                "pkg: im Manifest genannt, aber nicht im Archiv: {}",
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(ExtractReport {
            files: count,
            bytes: total,
        })
    }
}

fn entry_path<R: Read>(entry: &tar::Entry<'_, R>) -> Result<String> {
    let p = entry
        .path()
        .map_err(|e| SeppError::Config(format!("pkg: Eintragspfad: {e}")))?;
    p.to_str()
        .map(|s| s.to_string())
        .ok_or_else(|| SeppError::Config("pkg: Eintragspfad ist kein UTF-8".into()))
}

/// Liest den nächsten Eintrag, der genau `name` heißen muss, bis `max` Bytes.
fn read_named<R: Read>(entries: &mut tar::Entries<'_, R>, name: &str, max: u64) -> Result<Vec<u8>> {
    let entry = entries
        .next()
        .ok_or_else(|| SeppError::Config(format!("pkg: Archiv endet vor {name}")))?
        .map_err(|e| SeppError::Config(format!("pkg: {name}: {e}")))?;
    let path = entry_path(&entry)?;
    if path != name {
        return Err(SeppError::Config(format!(
            "pkg: erwartet {name} als Eintrag, gefunden {path:?} — manifest.toml und \
             manifest.sig müssen die ersten Einträge sein"
        )));
    }
    if entry.header().entry_type() != tar::EntryType::Regular {
        return Err(SeppError::Config(format!("pkg: {name} ist keine Datei")));
    }
    if entry.size() > max {
        return Err(SeppError::Config(format!(
            "pkg: {name} ist größer als {max} Bytes"
        )));
    }
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry
        .take(max + 1)
        .read_to_end(&mut buf)
        .map_err(|e| SeppError::Config(format!("pkg: {name}: {e}")))?;
    if buf.len() as u64 > max {
        return Err(SeppError::Config(format!(
            "pkg: {name} ist größer als {max} Bytes"
        )));
    }
    Ok(buf)
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Baukasten für Tests im ganzen Crate: ein Quellverzeichnis und ein gepacktes Paket.
    use super::*;

    pub const PLUGIN_TOML: &str = "name = \"zaehler\"\nabi = 1\n[capabilities]\nfs_read = [\"./\"]\nnet = [\"api.example.com\"]\nenv = [\"ACME_TOKEN\"]\n";

    /// Ein vollständiges Quellverzeichnis: Skill, Prompt, Hook, Fake-Plugin mit Manifest.
    pub fn source_dir(dir: &Path, name: &str, version: &str, rights: &str) {
        std::fs::create_dir_all(dir.join("skills/rechnung")).unwrap();
        std::fs::create_dir_all(dir.join("prompts")).unwrap();
        std::fs::create_dir_all(dir.join("hooks")).unwrap();
        std::fs::create_dir_all(dir.join("plugins")).unwrap();
        std::fs::write(
            dir.join("skills/rechnung/SKILL.md"),
            "# Rechnung\nPrüfe §14 UStG.\n",
        )
        .unwrap();
        std::fs::write(dir.join("prompts/pruefen.md"), "Prüfe die Rechnung.\n").unwrap();
        std::fs::write(
            dir.join("hooks/log.rhai"),
            "fn on_tool_call(ctx) { continue_() }\n",
        )
        .unwrap();
        std::fs::write(dir.join("plugins/zaehler.wasm"), b"\0asm\x01\0\0\0fake").unwrap();
        std::fs::write(dir.join("plugins/zaehler.toml"), PLUGIN_TOML).unwrap();
        std::fs::write(dir.join("README.md"), "# Demo\n").unwrap();
        std::fs::write(
            dir.join(MANIFEST),
            format!(
                "# Kommentar bleibt\nformat = 1\nname = \"{name}\"\nversion = \"{version}\"\n\
                 description = \"Demo\"\n\n[publisher]\nname = \"acme\"\n\n\
                 [vars.BELEGE_DIR]\ndescription = \"Ordner mit den Belegen\"\nkind = \"path\"\n\
                 default = \"~/buchhaltung\"\n\n{rights}"
            ),
        )
        .unwrap();
    }

    pub const RIGHTS: &str = "[rights.zaehler]\nfs_read = [\"${BELEGE_DIR}\"]\nnet = [\"api.example.com\"]\nenv = [\"ACME_TOKEN\"]\n";

    /// Packt ein Quellverzeichnis mit einem frischen Schlüssel; liefert Paketpfad und Schlüssel.
    pub fn packed(tmp: &Path, name: &str, version: &str, rights: &str) -> (PathBuf, SigningKey) {
        let src = tmp.join(format!("src-{name}-{version}"));
        source_dir(&src, name, version, rights);
        let (key, _) = SigningKey::generate().unwrap();
        let out = tmp.join(format!("{name}-{version}.seppkg"));
        pack_dir(&src, &key, &out).unwrap();
        (out, key)
    }

    /// Ein Archiv von Hand, für Manipulationen: `entries` = (Pfad, Inhalt).
    pub fn raw_archive(out: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(out).unwrap();
        let enc = zstd::stream::write::Encoder::new(file, 3).unwrap();
        let mut tar = tar::Builder::new(enc);
        for (name, bytes) in entries {
            append_bytes(&mut tar, name, bytes).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }

    /// Signiertes Manifest + Signatur als Einträge, für `raw_archive`.
    pub fn signed_head(manifest: &str, key: &SigningKey) -> (String, String) {
        let sig = key.sign(manifest.as_bytes());
        (
            manifest.to_string(),
            format!("{}\n", crypto::encode_signature(&sig)),
        )
    }

    /// Ein Manifest-Text mit gegebenem Schlüssel und Dateiliste.
    pub fn manifest_text(key: &SigningKey, files: &[(&str, &[u8])]) -> String {
        let mut s = format!(
            "format = 1\nname = \"demo\"\nversion = \"1.0.0\"\n[publisher]\nname = \"acme\"\nkey = \"{}\"\n[files]\n",
            crypto::encode_pubkey(&key.public_key())
        );
        for (path, bytes) in files {
            s.push_str(&format!("\"{path}\" = \"{}\"\n", crypto::sha256_hex(bytes)));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn pack_then_extract_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let (out, key) = packed(tmp.path(), "demo", "1.0.0", RIGHTS);
        let archive = PkgArchive::open(&out).unwrap();
        let signed = archive.read_signed_manifest().unwrap();
        assert_eq!(signed.manifest.name, "demo");
        assert_eq!(signed.fingerprint, key.fingerprint());
        assert!(
            signed.manifest_text.starts_with("# Kommentar bleibt"),
            "kommentarerhaltend"
        );
        assert_eq!(signed.manifest.files.len(), 6);
        assert_eq!(
            signed.manifest.rights["zaehler"].env,
            vec!["ACME_TOKEN".to_string()]
        );

        let dest = tmp.path().join("dest");
        let report = archive.extract_verified(&signed, &dest).unwrap();
        assert_eq!(report.files, 6);
        assert_eq!(
            std::fs::read_to_string(dest.join("skills/rechnung/SKILL.md")).unwrap(),
            "# Rechnung\nPrüfe §14 UStG.\n"
        );
        assert!(dest.join("plugins/zaehler.wasm").is_file());
        assert!(
            !dest.join(MANIFEST).exists(),
            "Manifest wird nicht entpackt"
        );

        // Reproduzierbar: zweimal packen, gleiche Bytes.
        let out2 = tmp.path().join("again.seppkg");
        pack_dir(&tmp.path().join("src-demo-1.0.0"), &key, &out2).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&out2).unwrap());
    }

    #[test]
    fn pack_rejects_symlinks_foreign_toplevel_and_existing_files_section() {
        let tmp = tempfile::tempdir().unwrap();
        let (key, _) = SigningKey::generate().unwrap();

        let src = tmp.path().join("fremd");
        source_dir(&src, "demo", "1.0.0", "");
        std::fs::create_dir_all(src.join("policy")).unwrap();
        std::fs::write(src.join("policy/x.toml"), "").unwrap();
        let e = pack_dir(&src, &key, &tmp.path().join("a.seppkg")).unwrap_err();
        assert!(e.to_string().contains("gehört nicht"), "{e}");

        #[cfg(unix)]
        {
            let src = tmp.path().join("link");
            source_dir(&src, "demo", "1.0.0", "");
            std::os::unix::fs::symlink("/etc/passwd", src.join("skills/passwd.md")).unwrap();
            let e = pack_dir(&src, &key, &tmp.path().join("b.seppkg")).unwrap_err();
            assert!(e.to_string().contains("Symlink"), "{e}");
        }

        let src = tmp.path().join("files");
        source_dir(&src, "demo", "1.0.0", "");
        let mut m = std::fs::read_to_string(src.join(MANIFEST)).unwrap();
        m.push_str("[files]\n\"x\" = \"y\"\n");
        std::fs::write(src.join(MANIFEST), m).unwrap();
        let e = pack_dir(&src, &key, &tmp.path().join("c.seppkg")).unwrap_err();
        assert!(e.to_string().contains("[files]"), "{e}");

        // Plugin-Manifest mit anderem Namen als der Dateistamm.
        let src = tmp.path().join("name");
        source_dir(&src, "demo", "1.0.0", "");
        std::fs::write(src.join("plugins/zaehler.toml"), "name = \"anders\"\n").unwrap();
        let e = pack_dir(&src, &key, &tmp.path().join("d.seppkg")).unwrap_err();
        assert!(e.to_string().contains("Dateistamm"), "{e}");
    }

    #[test]
    fn open_rejects_wrong_magic_and_manifest_must_come_first() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("x.seppkg");
        std::fs::write(&p, b"kein paket").unwrap();
        assert!(PkgArchive::open(&p)
            .unwrap_err()
            .to_string()
            .contains("kein .seppkg"));

        let (key, _) = SigningKey::generate().unwrap();
        let files: &[(&str, &[u8])] = &[("README.md", b"# x\n")];
        let (m, s) = signed_head(&manifest_text(&key, files), &key);
        raw_archive(
            &p,
            &[
                ("README.md", b"# x\n"),
                ("manifest.toml", m.as_bytes()),
                ("manifest.sig", s.as_bytes()),
            ],
        );
        let e = PkgArchive::open(&p)
            .unwrap()
            .read_signed_manifest()
            .unwrap_err();
        assert!(e.to_string().contains("ersten Einträge"), "{e}");
    }

    #[test]
    fn tampered_signature_hash_and_unlisted_or_missing_files_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (key, _) = SigningKey::generate().unwrap();
        let readme: &[u8] = b"# x\n";
        let files: &[(&str, &[u8])] = &[("README.md", readme)];
        let text = manifest_text(&key, files);

        // Manipuliertes Manifest → Signatur ungültig.
        let (_, s) = signed_head(&text, &key);
        let p = tmp.path().join("sig.seppkg");
        raw_archive(
            &p,
            &[
                ("manifest.toml", text.replace("1.0.0", "9.9.9").as_bytes()),
                ("manifest.sig", s.as_bytes()),
                ("README.md", readme),
            ],
        );
        let e = PkgArchive::open(&p)
            .unwrap()
            .read_signed_manifest()
            .unwrap_err();
        assert!(e.to_string().contains("Signatur ungültig"), "{e}");

        // Datei verändert → Hash stimmt nicht.
        let (m, s) = signed_head(&text, &key);
        let p = tmp.path().join("hash.seppkg");
        raw_archive(
            &p,
            &[
                ("manifest.toml", m.as_bytes()),
                ("manifest.sig", s.as_bytes()),
                ("README.md", b"# anders\n"),
            ],
        );
        let a = PkgArchive::open(&p).unwrap();
        let signed = a.read_signed_manifest().unwrap();
        let e = a
            .extract_verified(&signed, &tmp.path().join("d1"))
            .unwrap_err();
        assert!(e.to_string().contains("SHA-256"), "{e}");

        // Datei nicht in [files].
        let p = tmp.path().join("extra.seppkg");
        raw_archive(
            &p,
            &[
                ("manifest.toml", m.as_bytes()),
                ("manifest.sig", s.as_bytes()),
                ("README.md", readme),
                ("hooks/x.rhai", b""),
            ],
        );
        let a = PkgArchive::open(&p).unwrap();
        let signed = a.read_signed_manifest().unwrap();
        let e = a
            .extract_verified(&signed, &tmp.path().join("d2"))
            .unwrap_err();
        assert!(e.to_string().contains("nicht in [files]"), "{e}");

        // Datei fehlt im Archiv.
        let p = tmp.path().join("missing.seppkg");
        raw_archive(
            &p,
            &[
                ("manifest.toml", m.as_bytes()),
                ("manifest.sig", s.as_bytes()),
            ],
        );
        let a = PkgArchive::open(&p).unwrap();
        let signed = a.read_signed_manifest().unwrap();
        let e = a
            .extract_verified(&signed, &tmp.path().join("d3"))
            .unwrap_err();
        assert!(e.to_string().contains("nicht im Archiv"), "{e}");
    }

    #[test]
    fn traversal_and_symlink_entries_are_refused_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let (key, _) = SigningKey::generate().unwrap();
        let body: &[u8] = b"x";
        // Manifest listet einen harmlosen Pfad; das Archiv liefert einen bösen unter gleichem Hash.
        let text = manifest_text(&key, &[("README.md", body)]);
        let (m, s) = signed_head(&text, &key);

        for bad in ["../evil", "/etc/evil", "skills/../../evil"] {
            let p = tmp
                .path()
                .join(format!("{}.seppkg", bad.replace(['/', '.'], "_")));
            let file = File::create(&p).unwrap();
            let enc = zstd::stream::write::Encoder::new(file, 3).unwrap();
            let mut tar = tar::Builder::new(enc);
            append_bytes(&mut tar, MANIFEST, m.as_bytes()).unwrap();
            append_bytes(&mut tar, SIGNATURE, s.as_bytes()).unwrap();
            // tar normalisiert Pfade beim Anhängen — der Header wird deshalb roh gesetzt.
            let mut header = tar::Header::new_gnu();
            fill_header(&mut header, 1);
            header.as_gnu_mut().unwrap().name[..bad.len()].copy_from_slice(bad.as_bytes());
            header.set_cksum();
            tar.append(&header, body).unwrap();
            tar.into_inner().unwrap().finish().unwrap();

            let a = PkgArchive::open(&p).unwrap();
            let signed = a.read_signed_manifest().unwrap();
            let dest = tmp.path().join("dest");
            let e = a.extract_verified(&signed, &dest).unwrap_err();
            assert!(!e.to_string().is_empty(), "{bad}");
            assert!(!tmp.path().join("evil").exists() && !Path::new("/etc/evil").exists());
            let _ = std::fs::remove_dir_all(&dest);
        }

        // Symlink-Eintrag.
        let p = tmp.path().join("symlink.seppkg");
        let file = File::create(&p).unwrap();
        let enc = zstd::stream::write::Encoder::new(file, 3).unwrap();
        let mut tar = tar::Builder::new(enc);
        append_bytes(&mut tar, MANIFEST, m.as_bytes()).unwrap();
        append_bytes(&mut tar, SIGNATURE, s.as_bytes()).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        tar.append_link(&mut header, "README.md", "/etc/passwd")
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        let a = PkgArchive::open(&p).unwrap();
        let signed = a.read_signed_manifest().unwrap();
        let e = a
            .extract_verified(&signed, &tmp.path().join("dsym"))
            .unwrap_err();
        assert!(
            e.to_string().contains("Symlink") || e.to_string().contains("nicht erlaubt"),
            "{e}"
        );
    }

    #[test]
    fn oversized_header_is_refused_before_reading() {
        let tmp = tempfile::tempdir().unwrap();
        let (key, _) = SigningKey::generate().unwrap();
        let text = manifest_text(&key, &[("README.md", b"x")]);
        let (m, s) = signed_head(&text, &key);
        let p = tmp.path().join("big.seppkg");
        let file = File::create(&p).unwrap();
        let enc = zstd::stream::write::Encoder::new(file, 3).unwrap();
        let mut tar = tar::Builder::new(enc);
        append_bytes(&mut tar, MANIFEST, m.as_bytes()).unwrap();
        append_bytes(&mut tar, SIGNATURE, s.as_bytes()).unwrap();
        let mut header = tar::Header::new_gnu();
        fill_header(&mut header, MAX_FILE_BYTES + 1);
        header.set_path("README.md").unwrap();
        header.set_cksum();
        // Nur den Header schreiben — den angekündigten Inhalt gibt es nicht.
        tar.get_mut().write_all(header.as_bytes()).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        let a = PkgArchive::open(&p).unwrap();
        let signed = a.read_signed_manifest().unwrap();
        let e = a
            .extract_verified(&signed, &tmp.path().join("d"))
            .unwrap_err();
        assert!(e.to_string().contains("überschreiten"), "{e}");
    }
}
