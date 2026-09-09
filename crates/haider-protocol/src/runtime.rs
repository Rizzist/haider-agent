//! Typed, in-process runtime settings. These are not RPC schema additions.

/// SQLite WAL commit policy, supplied explicitly by embedded hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreSynchronous {
    Normal,
    Full,
}

impl StoreSynchronous {
    #[must_use]
    pub const fn pragma_value(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Full => "FULL",
        }
    }
}

/// Immutable facts from the actual store generation and bound RPC endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonBootstrapMetadata {
    pub daemon_generation: u64,
    pub endpoint_path: std::path::PathBuf,
}
