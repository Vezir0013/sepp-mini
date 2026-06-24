//! `sepp-policy` — Capabilities, Policy-Auswertung und OS-Sandbox.
//!
//! Default ist **deny**: was nicht deklariert ist, gibt es nicht (`docs/06-security-model.md`).
//! Eine [`Policy`] ist die Menge gewährter [`Capability`]s, gelesen aus einem [`Manifest`]
//! (`specs/extension-manifest.md`). Durchsetzung out-of-process über [`Sandbox`] (Linux:
//! Landlock; sonst portabler Fallback ohne Durchsetzung + Warnung).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use sepp_core::{Result, SeppError};

mod sandbox;
mod secrets;
#[cfg(target_os = "linux")]
pub use sandbox::LandlockSandbox;
pub use sandbox::{default_sandbox, NullSandbox, Sandbox};
pub use secrets::SecretBroker;

/// Ein atomares Recht.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    FsRead {
        prefix: PathBuf,
    },
    FsWrite {
        prefix: PathBuf,
    },
    /// Host exakt (`api.example.com`) oder Glob (`*.example.com`).
    Net {
        host: String,
    },
    Env {
        name: String,
    },
    Exec {
        program: String,
    },
}

/// Die Menge gewährter Capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    pub granted: Vec<Capability>,
}

impl Policy {
    pub fn new(granted: Vec<Capability>) -> Self {
        Policy { granted }
    }

    /// Erlaubt die Policy das angefragte Recht? Default deny.
    pub fn allows(&self, requested: &Capability) -> bool {
        self.granted.iter().any(|g| covers(g, requested))
    }

    /// Lese-Pfad-Präfixe (für die Sandbox).
    pub fn fs_read_prefixes(&self) -> Vec<PathBuf> {
        self.granted
            .iter()
            .filter_map(|c| match c {
                Capability::FsRead { prefix } => Some(prefix.clone()),
                _ => None,
            })
            .collect()
    }

    /// Schreib-Pfad-Präfixe (für die Sandbox).
    pub fn fs_write_prefixes(&self) -> Vec<PathBuf> {
        self.granted
            .iter()
            .filter_map(|c| match c {
                Capability::FsWrite { prefix } => Some(prefix.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Deckt das gewährte Recht `g` das angefragte `r`?
fn covers(g: &Capability, r: &Capability) -> bool {
    use Capability::*;
    match (g, r) {
        (FsRead { prefix: gp }, FsRead { prefix: rp }) => rp.starts_with(gp),
        (FsWrite { prefix: gp }, FsWrite { prefix: rp }) => rp.starts_with(gp),
        (Net { host: gh }, Net { host: rh }) => host_matches(gh, rh),
        (Env { name: gn }, Env { name: rn }) => gn == rn,
        (Exec { program: gp }, Exec { program: rp }) => gp == rp,
        _ => false,
    }
}

/// `*.example.com` matcht Subdomains (`api.example.com`), nicht den Apex; sonst exakt.
fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(&format!(".{suffix}")),
        None => pattern == host,
    }
}

/// Manifest einer code-führenden Erweiterung (`manifest.toml`).
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    /// `"wasm"` | `"mcp"`.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Nur `kind = "wasm"`.
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub capabilities: Capabilities,
}

/// Deklarierte Capabilities (Manifest- bzw. `[mcp.servers.capabilities]`-Form).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub fs_read: Vec<String>,
    #[serde(default)]
    pub fs_write: Vec<String>,
    #[serde(default)]
    pub net: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub exec: Vec<String>,
}

impl Capabilities {
    /// Baut eine [`Policy`]; Pfade werden (`~`/relativ aufgelöst und) kanonisiert.
    pub fn to_policy(&self) -> Policy {
        let mut granted = Vec::new();
        for p in &self.fs_read {
            granted.push(Capability::FsRead {
                prefix: resolve_path(p),
            });
        }
        for p in &self.fs_write {
            granted.push(Capability::FsWrite {
                prefix: resolve_path(p),
            });
        }
        for h in &self.net {
            granted.push(Capability::Net { host: h.clone() });
        }
        for e in &self.env {
            granted.push(Capability::Env { name: e.clone() });
        }
        for x in &self.exec {
            granted.push(Capability::Exec { program: x.clone() });
        }
        Policy { granted }
    }
}

impl Manifest {
    pub fn parse(toml_str: &str) -> Result<Manifest> {
        toml::from_str(toml_str).map_err(|e| SeppError::Config(format!("manifest: {e}")))
    }

    pub fn from_file(path: &Path) -> Result<Manifest> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| SeppError::Config(format!("manifest {}: {e}", path.display())))?;
        Self::parse(&text)
    }

    pub fn policy(&self) -> Policy {
        self.capabilities.to_policy()
    }
}

/// Löst `~` (Home) und relative `./`-Pfade (gegen cwd) auf und kanonisiert best-effort.
fn resolve_path(p: &str) -> PathBuf {
    let expanded: PathBuf = if let Some(rest) = p.strip_prefix('~') {
        match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest.trim_start_matches('/')),
            None => PathBuf::from(p),
        }
    } else {
        PathBuf::from(p)
    };
    let abs = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map(|c| c.join(&expanded))
            .unwrap_or(expanded)
    };
    abs.canonicalize().unwrap_or(abs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn fs_prefix_allows_subpaths_only() {
        let pol = Policy::new(vec![Capability::FsWrite {
            prefix: p("/proj/out"),
        }]);
        assert!(pol.allows(&Capability::FsWrite {
            prefix: p("/proj/out/sub/x")
        }));
        assert!(pol.allows(&Capability::FsWrite {
            prefix: p("/proj/out")
        }));
        assert!(!pol.allows(&Capability::FsWrite {
            prefix: p("/proj/other")
        }));
        // FsWrite-Grant ist kein FsRead-Grant.
        assert!(!pol.allows(&Capability::FsRead {
            prefix: p("/proj/out/x")
        }));
    }

    #[test]
    fn net_glob_matches_subdomains_not_apex() {
        let pol = Policy::new(vec![
            Capability::Net {
                host: "*.example.com".into(),
            },
            Capability::Net {
                host: "api.foo.com".into(),
            },
        ]);
        assert!(pol.allows(&Capability::Net {
            host: "a.example.com".into()
        }));
        assert!(pol.allows(&Capability::Net {
            host: "x.y.example.com".into()
        }));
        assert!(!pol.allows(&Capability::Net {
            host: "example.com".into()
        }));
        assert!(pol.allows(&Capability::Net {
            host: "api.foo.com".into()
        }));
        assert!(!pol.allows(&Capability::Net {
            host: "evil.com".into()
        }));
    }

    #[test]
    fn env_and_exec_exact() {
        let pol = Policy::new(vec![
            Capability::Env {
                name: "TOKEN".into(),
            },
            Capability::Exec {
                program: "git".into(),
            },
        ]);
        assert!(pol.allows(&Capability::Env {
            name: "TOKEN".into()
        }));
        assert!(!pol.allows(&Capability::Env {
            name: "OTHER".into()
        }));
        assert!(pol.allows(&Capability::Exec {
            program: "git".into()
        }));
        assert!(!pol.allows(&Capability::Exec {
            program: "rm".into()
        }));
    }

    #[test]
    fn empty_policy_denies_everything() {
        let pol = Policy::default();
        assert!(!pol.allows(&Capability::Net {
            host: "x.com".into()
        }));
        assert!(!pol.allows(&Capability::FsRead { prefix: p("/") }));
    }

    #[test]
    fn manifest_parses_capabilities() {
        let toml = r#"
            name = "string-tools"
            version = "0.1.0"
            kind = "wasm"
            entry = "string_tools.wasm"

            [capabilities]
            fs_read = ["/abs/read"]
            net = ["api.example.com", "*.cdn.example.com"]
            env = ["LEVEL"]
        "#;
        let m = Manifest::parse(toml).unwrap();
        assert_eq!(m.name, "string-tools");
        assert_eq!(m.kind.as_deref(), Some("wasm"));
        let pol = m.policy();
        assert!(pol.allows(&Capability::FsRead {
            prefix: p("/abs/read/x")
        }));
        assert!(pol.allows(&Capability::Net {
            host: "sub.cdn.example.com".into()
        }));
        assert!(pol.allows(&Capability::Env {
            name: "LEVEL".into()
        }));
        assert!(!pol.allows(&Capability::Net {
            host: "evil.com".into()
        }));
    }

    #[test]
    fn manifest_without_capabilities_is_pure() {
        let m = Manifest::parse("name=\"reverse\"\nkind=\"wasm\"\nentry=\"r.wasm\"").unwrap();
        assert!(m.policy().granted.is_empty());
    }
}
