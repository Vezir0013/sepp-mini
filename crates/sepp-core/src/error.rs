//! Zentraler Fehlertyp. Library-Crates geben immer `Result<T>` zurück — niemals `panic!`.

use thiserror::Error;

/// Fehlerklassen über alle `sepp-*`-Crates hinweg.
///
/// Fremdfehler werden mit Kontext in die String-Varianten gemappt
/// (z. B. `SeppError::Provider("anthropic: 429 rate limited")`), damit die
/// Meldung am LLM/Log brauchbar ist.
#[derive(Debug, Error)]
pub enum SeppError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("capability denied: {0}")]
    CapabilityDenied(String),
    #[error("session error: {0}")]
    Session(String),
    #[error("config error: {0}")]
    Config(String),
    #[error("serialization error")]
    Serde(#[from] serde_json::Error),
    #[error("io error")]
    Io(#[from] std::io::Error),
    #[error("aborted")]
    Aborted,
}

/// Projektweiter `Result`-Alias.
pub type Result<T> = std::result::Result<T, SeppError>;
