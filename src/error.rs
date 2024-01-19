use std::array::TryFromSliceError;
use std::fmt::Formatter;

use kanal::{ReceiveError, SendError};
use prost::Message;
use tokio::io;
use tokio::sync::AcquireError;

/// generic error type for cccp
#[derive(Debug)]
pub(crate) struct Error {
    pub(crate) kind: ErrorKind,
}

#[derive(Debug)]
pub(crate) enum ErrorKind {
    Io(io::Error),
    AddrParse(std::net::AddrParseError),
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
    AsyncSsh(async_ssh2_tokio::Error),
    RuSsh(russh::Error),
    Base64Decode(base64::DecodeError),
    Octocrab(octocrab::Error),
    Sftp(russh_sftp::client::error::Error),
    MissingQueue,
    MaxRetries,
    #[cfg(windows)]
    StatusError,
    /// a file transfer failure with reason
    Failure(u32),
    EmptyPath,
    InvalidExtension,
    /// an unexpected protobuf message was encountered
    UnexpectedMessage(Box<dyn Message>),
    /// the command did not finish
    NoExitStatus,
    /// the cccp command was not found on the remote host
    CommandNotFound,
    /// the command failed with a non-zero exit status
    CommandFailed(u32),
    /// os identification failed on the remote host
    UnknownOs((String, String)),
    /// no suitable release was found on github
    NoSuitableRelease,
    /// the file was not found in the archive
    FileNotFound,
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
            kind: ErrorKind::AddrParse(error),
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

impl From<async_ssh2_tokio::Error> for Error {
    fn from(error: async_ssh2_tokio::Error) -> Self {
        Self {
            kind: ErrorKind::AsyncSsh(error),
        }
    }
}

impl From<russh::Error> for Error {
    fn from(error: russh::Error) -> Self {
        Self {
            kind: ErrorKind::RuSsh(error),
        }
    }
}

impl From<base64::DecodeError> for Error {
    fn from(error: base64::DecodeError) -> Self {
        Self {
            kind: ErrorKind::Base64Decode(error),
        }
    }
}

impl From<octocrab::Error> for Error {
    fn from(error: octocrab::Error) -> Self {
        Self {
            kind: ErrorKind::Octocrab(error),
        }
    }
}

impl From<russh_sftp::client::error::Error> for Error {
    fn from(error: russh_sftp::client::error::Error) -> Self {
        Self {
            kind: ErrorKind::Sftp(error),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            ErrorKind::Io(ref error) => write!(f, "IO error: {}", error),
            ErrorKind::AddrParse(ref error) => write!(f, "Address parse error: {}", error),
            ErrorKind::Decode(ref error) => write!(f, "Decode error: {}", error),
            ErrorKind::Join(ref error) => write!(f, "Join error: {}", error),
            ErrorKind::Send(ref error) => write!(f, "Send error: {}", error),
            ErrorKind::Receive(ref error) => write!(f, "Receive error: {}", error),
            ErrorKind::Acquire(ref error) => write!(f, "Acquire error: {}", error),
            ErrorKind::TryFromSlice(ref error) => write!(f, "TryFromSlice error: {}", error),
            #[cfg(windows)]
            ErrorKind::ContainsNull(ref error) => write!(f, "contains null error: {}", error),
            #[cfg(unix)]
            ErrorKind::Nix(ref error) => write!(f, "nix error: {}", error),
            ErrorKind::StripPrefix(ref error) => write!(f, "strip prefix error: {}", error),
            ErrorKind::AsyncSsh(ref error) => write!(f, "ssh error: {}", error),
            ErrorKind::RuSsh(ref error) => write!(f, "ssh error: {}", error),
            ErrorKind::Base64Decode(ref error) => write!(f, "base64 decode error: {}", error),
            ErrorKind::Octocrab(ref error) => write!(f, "octocrab error: {}", error),
            ErrorKind::Sftp(ref error) => write!(f, "sftp error: {}", error),
            ErrorKind::MissingQueue => write!(f, "missing queue"),
            ErrorKind::MaxRetries => write!(f, "max retries"),
            #[cfg(windows)]
            ErrorKind::StatusError => write!(f, "status error"),
            ErrorKind::Failure(ref reason) => write!(f, "failure: {}", reason),
            ErrorKind::EmptyPath => write!(f, "empty path"),
            ErrorKind::InvalidExtension => write!(f, "invalid extension"),
            ErrorKind::UnexpectedMessage(ref message) => {
                write!(f, "unexpected message {:?}", message)
            }
            ErrorKind::NoExitStatus => write!(f, "no exit status"),
            ErrorKind::CommandNotFound => write!(f, "cccp command not found"),
            ErrorKind::CommandFailed(ref status) => {
                write!(f, "command failed with status {}", status)
            }
            ErrorKind::UnknownOs((ref stdout, ref stderr)) => {
                write!(f, "unknown os {} | {}", stdout, stderr)
            }
            ErrorKind::NoSuitableRelease => {
                write!(f, "no suitable release for target, use --custom-binary")
            }
            ErrorKind::FileNotFound => write!(f, "file not found"),
        }
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

    pub(crate) fn invalid_extension() -> Self {
        Self {
            kind: ErrorKind::InvalidExtension,
        }
    }

    pub(crate) fn unexpected_message(message: Box<dyn Message>) -> Self {
        Self {
            kind: ErrorKind::UnexpectedMessage(message),
        }
    }

    pub(crate) fn no_exit_status() -> Self {
        Self {
            kind: ErrorKind::NoExitStatus,
        }
    }

    pub(crate) fn command_not_found() -> Self {
        Self {
            kind: ErrorKind::CommandNotFound,
        }
    }

    pub(crate) fn command_failed(status: u32) -> Self {
        Self {
            kind: ErrorKind::CommandFailed(status),
        }
    }

    pub(crate) fn unknown_os(stdout: String, stderr: String) -> Self {
        Self {
            kind: ErrorKind::UnknownOs((stdout, stderr)),
        }
    }

    pub(crate) fn no_suitable_release() -> Self {
        Self {
            kind: ErrorKind::NoSuitableRelease,
        }
    }

    pub(crate) fn file_not_found() -> Self {
        Self {
            kind: ErrorKind::FileNotFound,
        }
    }

    #[cfg(windows)]
    pub(crate) fn status_error() -> Self {
        Self {
            kind: ErrorKind::StatusError,
        }
    }
}
