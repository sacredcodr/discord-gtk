//! Result codes shared by the network layer and the UI.
//!
//! Codes are granular on purpose: the UI decides what to show (a toast, a
//! re-login prompt) from the variant, never by parsing a message string.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The request never produced a response (DNS, TLS, connection reset, timeout).
    Network(String),
    /// Discord answered with a non-success status.
    ///
    /// 401 means the token is bad, 403 means the account cannot see the
    /// resource (common for channels listed in a guild the user may not read).
    Http { status: u16, path: String },
    /// Still rate limited after `Rest::MAX_ATTEMPTS` attempts.
    RateLimited { path: String },
    /// The response body did not match the expected shape.
    Decode { path: String, reason: String },
    /// The gateway closed the connection with a code that retrying cannot fix
    /// (4004 authentication failed, 4010-4014 bad shard or intents).
    GatewayRejected { code: u16 },
}

impl Error {
    /// True when the stored token should be discarded and the user asked to log in again.
    pub fn is_auth_failure(&self) -> bool {
        matches!(
            self,
            Self::Http { status: 401, .. } | Self::GatewayRejected { code: 4004 }
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network(e) => write!(f, "Network error: {e}"),
            Self::Http { status: 401, .. } => write!(f, "Discord rejected the token"),
            Self::Http { status: 403, .. } => write!(f, "You do not have access to that"),
            Self::Http { status, path } => write!(f, "Discord returned {status} for {path}"),
            Self::RateLimited { path } => write!(f, "Rate limited on {path}"),
            Self::Decode { path, reason } => write!(f, "Unexpected response from {path}: {reason}"),
            Self::GatewayRejected { code: 4004 } => write!(f, "The gateway rejected the token"),
            Self::GatewayRejected { code } => write!(f, "The gateway closed the connection ({code})"),
        }
    }
}

impl std::error::Error for Error {}
