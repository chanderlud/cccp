use std::array::TryFromSliceError;
use std::process::{ExitCode, Termination};

use kanal::{ReceiveError, SendError};
use tokio::io;
use tokio::sync::AcquireError;

#[derive(Debug)]
pub(crate) struct Error {
    pub(crate) kind: ErrorKind,
}

#[derive(Debug)]
pub(crate) enum ErrorKind {
    Io(io::Error),
    Parse(std::net::AddrParseError),
    Decode(prost::DecodeError),
    Join(tokio::task::JoinError),
    Send(SendError),
    Receive(ReceiveError),
    Acquire(AcquireError),
    TryFromSlice(TryFromSliceError),
    #[cfg(windows)]
    ContainsNull(widestring::error::ContainsNul<u16>),
    #[cfg(unix)]
    Nix(nix::Error),
    StripPrefix(std::path::StripPrefixError),
    MissingQueue,
    MaxRetries,
    #[cfg(windows)]
    StatusError,
    Failure(u32),
    EmptyPath,
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self {
            kind: ErrorKind::Io(error),
        }
    }
}

impl From<std::net::AddrParseError> for Error {
    fn from(error: std::net::AddrParseError) -> Self {
        Self {
            kind: ErrorKind::Parse(error),
        }
    }
}

impl From<prost::DecodeError> for Error {
    fn from(error: prost::DecodeError) -> Self {
        Self {
            kind: ErrorKind::Decode(error),
        }
    }
}

impl From<tokio::task::JoinError> for Error {
    fn from(error: tokio::task::JoinError) -> Self {
        Self {
            kind: ErrorKind::Join(error),
        }
    }
}

impl From<SendError> for Error {
    fn from(error: SendError) -> Self {
        Self {
            kind: ErrorKind::Send(error),
        }
    }
}

impl From<ReceiveError> for Error {
    fn from(error: ReceiveError) -> Self {
        Self {
            kind: ErrorKind::Receive(error),
        }
    }
}

impl From<AcquireError> for Error {
    fn from(error: AcquireError) -> Self {
        Self {
            kind: ErrorKind::Acquire(error),
        }
    }
}

impl From<TryFromSliceError> for Error {
    fn from(error: TryFromSliceError) -> Self {
        Self {
            kind: ErrorKind::TryFromSlice(error),
        }
    }
}

#[cfg(windows)]
impl From<widestring::error::ContainsNul<u16>> for Error {
    fn from(error: widestring::error::ContainsNul<u16>) -> Self {
        Self {
            kind: ErrorKind::ContainsNull(error),
        }
    }
}

#[cfg(unix)]
impl From<nix::Error> for Error {
    fn from(error: nix::Error) -> Self {
        Self {
            kind: ErrorKind::Nix(error),
        }
    }
}

impl From<std::path::StripPrefixError> for Error {
    fn from(error: std::path::StripPrefixError) -> Self {
        Self {
            kind: ErrorKind::StripPrefix(error),
        }
    }
}

impl Termination for Error {
    fn report(self) -> ExitCode {
        ExitCode::from(match self.kind {
            ErrorKind::Io(error) => match error.kind() {
                io::ErrorKind::NotFound => 1,
                _ => 2,
            },
            ErrorKind::Parse(_) => 3,
            ErrorKind::Decode(_) => 4,
            ErrorKind::Join(_) => 5,
            ErrorKind::Send(_) => 6,
            ErrorKind::Receive(_) => 7,
            ErrorKind::Acquire(_) => 8,
            _ => 9,
        })
    }
}

impl Error {
    pub(crate) fn missing_queue() -> Self {
        Self {
            kind: ErrorKind::MissingQueue,
        }
    }

    pub(crate) fn max_retries() -> Self {
        Self {
            kind: ErrorKind::MaxRetries,
        }
    }

    pub(crate) fn failure(reason: u32) -> Self {
        Self {
            kind: ErrorKind::Failure(reason),
        }
    }

    pub(crate) fn empty_path() -> Self {
        Self {
            kind: ErrorKind::EmptyPath,
        }
    }

    #[cfg(windows)]
    pub(crate) fn status_error() -> Self {
        Self {
            kind: ErrorKind::StatusError,
        }
    }
}
