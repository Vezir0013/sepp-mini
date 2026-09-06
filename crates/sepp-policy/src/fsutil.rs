//! Dateihelfer für Dateien, an denen Rechte und Nachweise hängen: atomar schreiben, private
//! Verzeichnisse anlegen.
//!
//! `policy.toml`, `installed.json`, Herausgeber-Schlüssel — bei allen wäre eine halb
//! geschriebene Datei schlimmer als gar keine. Deshalb geht jeder Schreibvorgang über eine
//! temporäre Datei im selben Verzeichnis, `fsync` und `rename`: Entweder steht der alte Inhalt
//! oder der neue, nie ein Stück von beidem.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use sepp_core::{Result, SeppError};

/// Schreibt `content` atomar nach `path`: temporäre Datei daneben, `fsync`, `rename`.
///
/// `mode`: Dateirechte für die neue Datei (unix). `None` übernimmt den Modus einer vorhandenen
/// Zieldatei — eine `policy.toml` unter `/etc/sepp` bleibt so für alle lesbar — und lässt sonst
/// die umask entscheiden. Zeigt `path` auf einen Symlink, wird dessen Ziel beschrieben, damit
/// eine per Symlink verwaltete Datei nicht durch eine Kopie ersetzt wird.
pub fn write_atomic(path: &Path, content: &[u8], mode: Option<u32>) -> Result<()> {
    let target = resolve_symlink(path);
    let parent = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&parent)
        .map_err(|e| SeppError::Config(format!("{}: {e}", parent.display())))?;

    // Modus: gewünscht, sonst der der vorhandenen Datei, sonst umask.
    #[cfg(unix)]
    let mode = mode.or_else(|| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&target)
            .ok()
            .map(|m| m.permissions().mode() & 0o777)
    });

    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("datei");
    let tmp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(m) = mode {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(m);
    }
    #[cfg(not(unix))]
    let _ = mode;

    let result = (|| -> std::io::Result<()> {
        let mut f = opts.open(&tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &target)?;
        // Den Verzeichniseintrag selbst festschreiben — sonst kann nach einem Stromausfall die
        // alte Datei wieder da sein, obwohl `rename` längst zurückgekehrt war.
        if let Ok(dir) = File::open(&parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(SeppError::Config(format!("{}: {e}", target.display())));
    }
    Ok(())
}

/// Legt `dir` samt Eltern an und setzt `0700` (unix) — für Verzeichnisse mit Nachweisen und
/// Schlüsseln. Ein vorhandenes Verzeichnis wird nur im Modus angepasst, nie geleert.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| SeppError::Config(format!("{}: {e}", dir.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| SeppError::Config(format!("{}: {e}", dir.display())))?;
    }
    Ok(())
}

/// Folgt einem Symlink bis zum Ziel (nur wenn `path` selbst einer ist); sonst `path`.
fn resolve_symlink(path: &Path) -> PathBuf {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_leaves_no_temp_file_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/b/policy.toml");
        write_atomic(&p, b"x = 1\n", None).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "x = 1\n");
        let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_keeps_existing_mode_and_honors_explicit_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, "alt").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&p, b"neu", None).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "neu");

        let q = dir.path().join("secret");
        write_atomic(&q, b"s", Some(0o600)).unwrap();
        assert_eq!(
            std::fs::metadata(&q).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_follows_a_symlink_to_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        std::fs::write(&real, "alt").unwrap();
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, b"neu", None).unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "neu");
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_sets_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x/keys");
        ensure_private_dir(&p).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o700
        );
        ensure_private_dir(&p).unwrap();
    }
}
