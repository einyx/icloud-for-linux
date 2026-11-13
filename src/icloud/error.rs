//! Errors of the iCloud protocol layer.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The credentials were rejected.
    InvalidCredentials,
    /// A two-factor challenge is pending.
    TwoFactorRequired,
    /// The submitted code was wrong.
    InvalidCode,
    /// The session has to be re-established (`421`, `450`, `500`).
    AuthRequired,
    /// Apple requires the updated terms of service to be accepted.
    TermsNotAccepted,
    /// The requested path does not exist.
    NotFound,
    /// The node changed underneath us.
    Conflict(String),
    /// A structured error from the service.
    Api {
        status: u16,
        code: Option<String>,
        message: String,
    },
    /// The request could not be delivered.
    Transport(String),
    /// The service answered with something we do not understand.
    Protocol(String),
    /// Local I/O failed.
    Io(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidCredentials => write!(f, "invalid Apple ID or password"),
            Error::TwoFactorRequired => write!(f, "two-factor authentication required"),
            Error::InvalidCode => write!(f, "the verification code was rejected"),
            Error::AuthRequired => {
                write!(f, "the session expired, run `icloudctl auth` again")
            }
            Error::TermsNotAccepted => write!(
                f,
                "Apple requires the updated terms of service to be accepted: \
                 run `icloudctl auth --accept-terms`"
            ),
            Error::NotFound => write!(f, "not found"),
            Error::Conflict(msg) => write!(f, "conflict: {}", msg),
            Error::Api {
                status,
                code,
                message,
            } => match code {
                Some(code) => write!(f, "iCloud error {} ({}): {}", status, code, message),
                None => write!(f, "iCloud error {}: {}", status, message),
            },
            Error::Transport(msg) => write!(f, "network error: {}", msg),
            Error::Protocol(msg) => write!(f, "unexpected response: {}", msg),
            Error::Io(msg) => write!(f, "i/o error: {}", msg),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Protocol(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
