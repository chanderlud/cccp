use std::error::Error;
use std::fmt::{Display, Formatter};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::str::FromStr;

use aes::cipher::crypto_common::rand_core::{OsRng, RngCore};
use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine;
use bytesize::ByteSize;
use clap::Parser;
use log::LevelFilter;
use regex::Regex;
use tokio::io;

use crate::items::{Cipher, Crypto};
use crate::PACKET_SIZE;

#[derive(Parser, Clone, Debug)]
#[clap(
    name = "cccp",
    version,
    author,
    about = "A fast and secure file transfer utility"
)]
pub(crate) struct Options {
    // the user does not need to set this
    #[clap(long, hide = true, default_value = "local")]
    pub(crate) mode: Mode,

    /// The first port to use
    #[clap(short, long, default_value_t = 50000)]
    pub(crate) start_port: u16,

    /// The last port to use
    #[clap(short, long, default_value_t = 50099)]
    pub(crate) end_port: u16,

    /// The number of threads to use
    #[clap(short, long, default_value_t = 98)]
    pub(crate) threads: usize,

    /// The log level [debug, info, warn, error]
    #[clap(short, long, default_value = "warn")]
    pub(crate) log_level: LevelFilter,

    /// The rate to send data at [b, kb, mb, gb, tb]
    #[clap(short, long, default_value = "1mb")]
    rate: ByteSize,

    /// The maximum number of concurrent transfers
    #[clap(short, long, default_value_t = 100)]
    pub(crate) max: usize,

    /// Encrypt the control stream
    #[clap(short, long, default_value = "aes")]
    pub(crate) control_crypto: Crypto,

    /// Verify integrity of transfers using blake3
    #[clap(short, long)]
    pub(crate) verify: bool,

    /// Overwrite existing files
    #[clap(short, long)]
    pub(crate) overwrite: bool,

    /// Include subdirectories recursively
    #[clap(short = 'R', long)]
    pub(crate) recursive: bool,

    /// Optionally encrypt the data stream
    #[clap(short = 'S', long)]
    pub(crate) stream_crypto: Option<Crypto>,

    /// Manually specify the bind address
    #[clap(short, long)]
    pub(crate) bind_address: Option<IpAddr>,

    /// The source file or directory
    #[clap()]
    pub(crate) source: IoSpec,

    /// The destination file or directory
    #[clap()]
    pub(crate) destination: IoSpec,
}

impl Options {
    pub(crate) fn format_command(&self, sender: bool) -> String {
        let mode = if sender { "rr" } else { "rs" };

        let stream_crypto = if let Some(ref crypto) = self.stream_crypto {
            format!(" --stream-crypto {}", crypto)
        } else {
            String::new()
        };

        format!(
            "cccp --mode {} -s {} -e {} -t {} -l {} -r \"{}\"{} --control-crypto {}{}{} \"{}\" \"{}\"",
            mode,
            self.start_port,
            self.end_port,
            self.threads,
            self.log_level,
            self.rate,
            stream_crypto,
            self.control_crypto,
            if self.overwrite { " -o" } else { "" },
            if self.verify { " -v" } else { "" },
            self.source,
            self.destination
        )
    }

    pub(crate) fn pps(&self) -> u64 {
        self.rate.0 / PACKET_SIZE as u64
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Mode {
    Local,
    Remote(bool), // Remote(sender)
}

impl FromStr for Mode {
    type Err = OptionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "l" => Self::Local,
            "rr" => Self::Remote(false),
            "rs" => Self::Remote(true),
            "local" => Self::Local,
            "remote-receiver" => Self::Remote(false),
            "remote-sender" => Self::Remote(true),
            _ => return Err(Self::Err::unknown_mode()),
        })
    }
}

impl FromStr for Crypto {
    type Err = OptionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let captures = Regex::new("([A-Za-z\\d]+)(?::([A-Za-z0-9+/]+))?(?::([A-Za-z0-9+/]+))?")
            .unwrap() // infallible
            .captures(s)
            .ok_or(Self::Err::invalid_cipher_format())?;

        // unwrap is safe because the regex requires a cipher name
        let cipher_str = captures.get(1).unwrap().as_str().to_uppercase();
        let cipher = Cipher::from_str_name(&cipher_str).ok_or(Self::Err::invalid_cipher())?;

        let key = captures
            .get(2)
            .map(|m| STANDARD_NO_PAD.decode(m.as_str()))
            .transpose()? // propagate the decode error
            .unwrap_or_else(|| random_bytes(cipher.key_length()));

        let iv = captures
            .get(3)
            .map(|m| STANDARD_NO_PAD.decode(m.as_str()))
            .transpose()? // propagate the decode error
            .unwrap_or_else(|| random_bytes(cipher.iv_length()));

        if key.len() != cipher.key_length() {
            Err(Self::Err::invalid_key())
        } else if iv.len() != cipher.iv_length() {
            Err(Self::Err::invalid_iv())
        } else {
            Ok(Self {
                cipher: cipher as i32,
                key,
                iv,
            })
        }
    }
}

impl Display for Crypto {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let key = STANDARD_NO_PAD.encode(&self.key);
        let iv = STANDARD_NO_PAD.encode(&self.iv);

        write!(
            f,
            "{}:{}:{}",
            Cipher::try_from(self.cipher).map_err(|_| std::fmt::Error)?,
            key,
            iv
        )
    }
}

/// a file located anywhere
#[derive(Clone, Debug)]
pub(crate) struct IoSpec {
    pub(crate) file_path: PathBuf,
    pub(crate) host: Option<SocketAddr>,
    pub(crate) username: Option<String>,
}

impl FromStr for IoSpec {
    type Err = OptionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let captures = Regex::new("(?:([\\w-]+)@([\\w.-]+)(?::(\\d+))?:)?([ \\w/.-]+)")
            .unwrap() // infallible
            .captures(s)
            .ok_or(Self::Err::malformed_io_spec("Invalid IO spec"))?;

        let username = captures.get(1).map(|m| m.as_str().to_string());
        let port = captures
            .get(3)
            .map(|m| m.as_str().parse::<u16>())
            .transpose()
            .map_err(|_| Self::Err::invalid_port())?
            .unwrap_or(22);

        let host = captures
            .get(2)
            .map(|m| {
                let host = m.as_str();

                match host.parse() {
                    Ok(ip) => Ok(SocketAddr::new(ip, port)),
                    Err(_) => {
                        // if the host is not an ip address, try to resolve  it as a domain
                        format!("{}:{}", host, port)
                            .to_socket_addrs()?
                            .next()
                            .ok_or(Self::Err::no_such_host())
                    }
                }
            })
            .transpose()?;

        let file_path = captures
            .get(4)
            .unwrap() // unwrap is safe because the regex requires a file path
            .as_str()
            .parse()
            .unwrap(); // infallible

        Ok(Self {
            file_path,
            host,
            username,
        })
    }
}

impl Display for IoSpec {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let file_path = self.file_path.display();

        match (&self.username, &self.host) {
            (None, None) => write!(f, "{}", file_path),
            (None, Some(host)) => write!(f, "{}:{}", host, file_path),
            (Some(username), Some(host)) => write!(f, "{}@{}:{}", username, host, file_path),
            _ => Err(std::fmt::Error),
        }
    }
}

impl IoSpec {
    pub(crate) fn is_local(&self) -> bool {
        self.host.is_none() || (self.host.is_some() && self.file_path.exists())
    }
}

#[derive(Debug)]
pub struct OptionParseError {
    kind: ErrorKind,
}

#[derive(Debug)]
enum ErrorKind {
    Io(io::Error),
    Decode(base64::DecodeError),
    MalformedIoSpec(&'static str),
    UnknownMode,
    InvalidCipher,
    InvalidCipherFormat,
    InvalidKey,
    InvalidIv,
    NoSuchHost,
    InvalidPort,
}

#[allow(deprecated)]
impl Display for OptionParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match &self.kind {
                ErrorKind::Io(error) => error.description(),
                ErrorKind::Decode(error) => error.description(),
                ErrorKind::MalformedIoSpec(message) => message,
                ErrorKind::UnknownMode => "The mode can be either sender or receiver",
                ErrorKind::InvalidCipher => "Invalid cipher",
                ErrorKind::InvalidCipherFormat => "Invalid cipher format",
                ErrorKind::InvalidKey => "Invalid key",
                ErrorKind::InvalidIv => "Invalid IV",
                ErrorKind::NoSuchHost => "No such host",
                ErrorKind::InvalidPort => "Invalid port",
            }
        )
    }
}

impl Error for OptionParseError {}

impl From<io::Error> for OptionParseError {
    fn from(error: io::Error) -> Self {
        Self {
            kind: ErrorKind::Io(error),
        }
    }
}

impl From<base64::DecodeError> for OptionParseError {
    fn from(error: base64::DecodeError) -> Self {
        Self {
            kind: ErrorKind::Decode(error),
        }
    }
}

impl OptionParseError {
    fn invalid_key() -> Self {
        Self {
            kind: ErrorKind::InvalidKey,
        }
    }

    fn invalid_iv() -> Self {
        Self {
            kind: ErrorKind::InvalidIv,
        }
    }

    fn invalid_cipher() -> Self {
        Self {
            kind: ErrorKind::InvalidCipher,
        }
    }

    fn invalid_cipher_format() -> Self {
        Self {
            kind: ErrorKind::InvalidCipherFormat,
        }
    }

    fn malformed_io_spec(message: &'static str) -> Self {
        Self {
            kind: ErrorKind::MalformedIoSpec(message),
        }
    }

    fn no_such_host() -> Self {
        Self {
            kind: ErrorKind::NoSuchHost,
        }
    }

    fn unknown_mode() -> Self {
        Self {
            kind: ErrorKind::UnknownMode,
        }
    }

    fn invalid_port() -> Self {
        Self {
            kind: ErrorKind::InvalidPort,
        }
    }
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    OsRng.fill_bytes(&mut bytes);
    bytes
}
