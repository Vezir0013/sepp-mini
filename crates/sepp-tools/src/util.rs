//! Kleine Tool-Helfer.

use std::path::PathBuf;

/// Normalisiert einen vom Modell gelieferten Pfad: ein führendes `@` (das manche Modelle
/// anhängen) und umgebende Whitespaces werden entfernt.
pub fn normalize_path(p: &str) -> PathBuf {
    let p = p.trim();
    let p = p.strip_prefix('@').unwrap_or(p);
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_at_and_trims() {
        assert_eq!(
            normalize_path("  @src/main.rs "),
            PathBuf::from("src/main.rs")
        );
        assert_eq!(normalize_path("src/main.rs"), PathBuf::from("src/main.rs"));
    }
}
