//! Serialisiert Datei-Mutationen pro kanonischem Pfad.
//!
//! Tool-Calls laufen parallel; ohne Serialisierung überschreibt der letzte Schreibvorgang
//! andere. Da der Vertrag eine **freie Funktion** vorschreibt, ist hier prozessweiter
//! Zustand (ein Lock-Registry) nötig und gerechtfertigt — die einzige globale Mutable im Projekt.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::Mutex as AsyncMutex;

use sepp_core::Result;

type Registry = Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>;

fn registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Kanonischer Schlüssel, damit verschiedene Schreibweisen derselben Datei dieselbe Queue
/// teilen. Existiert die Datei, löst `canonicalize` Symlinks/`..` auf. Sonst wird der Pfad
/// **absolut** gemacht (cwd-relativ) und lexikalisch normalisiert — so kollidieren z. B.
/// `a/b.txt`, `./a/b.txt` und ein absoluter Spelling auf demselben Schlüssel, auch wenn die
/// Datei (oder ihr Verzeichnis) noch nicht existiert.
fn canonical_key(path: &Path) -> PathBuf {
    if let Ok(c) = path.canonicalize() {
        return c;
    }
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    lexical_normalize(&abs)
}

/// Entfernt `.` und löst `..` rein lexikalisch auf (ohne Dateisystemzugriff).
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Führt `f` unter einem pro-Pfad exklusiven (async) Lock aus.
pub async fn with_file_mutation_queue<F, T>(path: &Path, f: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send,
{
    let key = canonical_key(path);
    // std-Mutex nur kurz halten, um den per-Pfad-Lock zu holen — NICHT über das await.
    let lock = {
        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        reg.entry(key)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;
    f.await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn serializes_concurrent_mutations_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("counter.txt");
        tokio::fs::write(&path, "0").await.unwrap();

        static MAX_CONCURRENT: AtomicUsize = AtomicUsize::new(0);
        static CURRENT: AtomicUsize = AtomicUsize::new(0);

        let mut handles = Vec::new();
        for _ in 0..16 {
            let p = path.clone();
            handles.push(tokio::spawn(async move {
                with_file_mutation_queue(&p, async {
                    let now = CURRENT.fetch_add(1, Ordering::SeqCst) + 1;
                    MAX_CONCURRENT.fetch_max(now, Ordering::SeqCst);
                    // Read-modify-write mit einem await dazwischen.
                    let cur: u64 = tokio::fs::read_to_string(&p)
                        .await
                        .unwrap()
                        .parse()
                        .unwrap();
                    tokio::task::yield_now().await;
                    tokio::fs::write(&p, (cur + 1).to_string()).await.unwrap();
                    CURRENT.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        // Nie zwei gleichzeitig im kritischen Abschnitt …
        assert_eq!(MAX_CONCURRENT.load(Ordering::SeqCst), 1);
        // … und kein verlorenes Update.
        let final_val: u64 = tokio::fs::read_to_string(&path)
            .await
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(final_val, 16);
    }

    #[test]
    fn divergent_spellings_of_new_file_share_key() {
        // Datei existiert nicht → canonicalize scheitert; trotzdem müssen verschiedene
        // Schreibweisen denselben Schlüssel ergeben (sonst keine Serialisierung).
        let a = canonical_key(Path::new("does/not/exist/new.txt"));
        let b = canonical_key(Path::new("./does/not/exist/new.txt"));
        let c = canonical_key(Path::new("does/not/./exist/sub/../new.txt"));
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert!(a.is_absolute());
    }
}
