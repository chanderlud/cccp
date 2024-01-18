use std::error;
use std::fmt::{Display, Formatter};
use std::net::{SocketAddr, ToSocketAddrs};
use std::ops::Not;
use std::path::PathBuf;
use std::str::FromStr;

use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine;
use bytesize::ByteSize;
use clap::{Parser, Subcommand};
use log::LevelFilter;
use regex::Regex;
use tokio::io;

use crate::cipher::random_bytes;
use crate::items::{Cipher, Crypto};
use crate::PACKET_SIZE;

const HELP_HEADING: &str = "\x1B[1m\x1B[4mAbout\x1B[0m
  cccp is a fast, secure, and reliable file transfer utility

\x1B[1m\x1B[4mIoSpec\x1B[0m
  - [user@][host:{port:}]file
  - If no user is set for a remote host, the current user is used
  - If no port is provided, port 22 is used
  - At least one IoSpec should be remote

\x1B[1m\x1B[4mCiphers\x1B[0m
  - NONE
  - CHACHA8
  - CHAHA20
  - AES128
  - AES192
  - AES256

\x1B[1m\x1B[4mFirewall\x1B[0m
  - The first two ports are used for TCP streams which carry control messages
  - The remaining ports are UDP sockets which carry data";

const TRANSFER_HEADING: &str = "\x1B[1m\x1B[4mNote:\x1B[0m using the \x1B[1mtransfer\x1B[0m command is equivalent to using no command at all";

#[derive(Parser)]
#[clap(version, about = HELP_HEADING)]
#[command(propagate_version = true)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Commands>,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// Run `transfer --help` for more options
    #[clap(about = TRANSFER_HEADING)]
    Transfer(Box<Options>),
    /// Install cccp on a remote host
    Install {
        /// IoSpec for the remote host & install location
        remote: IoSpec,
    },
}

#[derive(Parser)]
pub(crate) struct Options {
    // the user does not need to set this
    #[clap(long, hide = true, default_value = "l")]
    pub(crate) mode: Mode,

    // set by the controller automatically
    #[clap(long, hide = true, default_value = "c")]
    pub(crate) stream_setup_mode: SetupMode,

    /// First port to use
    #[clap(short, long, default_value_t = 50000)]
    pub(crate) start_port: u16,

    /// Last port to use
    #[clap(short, long, default_value_t = 50009)]
    pub(crate) end_port: u16,

    /// Parallel data streams
    #[clap(short, long, default_value_t = 8)]
    pub(crate) threads: usize,

    /// Log level [debug, info, warn, error]
    #[clap(short, long, default_value = "warn")]
    pub(crate) log_level: LevelFilter,

    /// Data send rate [b, kb, mb, gb, tb]
    #[clap(short, long, default_value = "1mb")]
    rate: ByteSize,

    /// Maximum concurrent transfers
    #[clap(short, long, default_value_t = 100)]
    pub(crate) max: usize,

    /// Encrypt the control stream
    #[clap(short, long, default_value = "AES256")]
    pub(crate) control_crypto: Crypto,

    /// Encrypt the data stream
    #[clap(short = 'S', long, default_value = "CHACHA20")]
    pub(crate) stream_crypto: Crypto,

    /// Receive timeout in MS
    #[clap(short = 'T', long, default_value_t = 5_000)]
    pub(crate) receive_timeout: u64,

    /// Limit for concurrent jobs
    #[clap(short, long, default_value_t = 1_000)]
    pub(crate) job_limit: usize,

    /// Requeue interval in MS
    #[clap(short = 'i', long, default_value_t = 1_000)]
    pub(crate) requeue_interval: u64,

    /// Maximum number of send/receive retries
    #[clap(short = 'M', long, default_value_t = 10)]
    pub(crate) max_retries: usize,

    /// Command to execute cccp
    #[clap(short = 'E', long, default_value = "cccp")]
    command: String,

    /// How often to print progress in MS
    #[clap(short, long, default_value_t = 1_000)]
    pub(crate) progress_interval: u64,

    /// Verify integrity of transfers using blake3
    #[clap(short, long)]
    pub(crate) verify: bool,

    /// Overwrite existing files
    #[clap(short, long)]
    pub(crate) overwrite: bool,

    /// Include subdirectories recursively
    #[clap(short = 'R', long)]
    pub(crate) recursive: bool,

    /// Do not check destination's available storage
    #[clap(short, long)]
    pub(crate) force: bool,

    /// Forces the destination to be a directory
    #[clap(short, long)]
    pub(crate) directory: bool,

    /// Log to a file (default: stderr / local only)
    #[clap(short = 'L', long)]
    pub(crate) log_file: Option<PathBuf>,

    /// Source IoSpec (InSpec)
    pub(crate) source: IoSpec,

    /// Destination IoSpec (OutSpec)
    pub(crate) destination: IoSpec,
}

impl Options {
    /// Returns the command to run on the remote host
    pub(crate) fn format_command(&self, sender: bool, mode: SetupMode) -> String {
        let mut arguments = vec![
            self.command.clone(),
            format!("--mode {}", if sender { "rr" } else { "rs" }),
            format!("--stream-setup-mode {}", mode),
            format!("-s {}", self.start_port),
            format!("-e {}", self.end_port),
            format!("-t {}", self.threads),
            format!("-l {}", self.log_level),
            format!("-r \"{}\"", self.rate),
            format!("-m {}", self.max),
            format!("-T {}", self.receive_timeout),
            format!("-j {}", self.job_limit),
            format!("-c {}", self.control_crypto),
            format!("-S {}", self.stream_crypto),
            format!("-i {}", self.requeue_interval),
            format!("-M {}", self.max_retries),
            format!("\"{}\"", self.source),
            format!("\"{}\"", self.destination),
        ];

        // optional arguments are inserted at index 1 so they are always before the IoSpecs
        if self.overwrite {
            arguments.insert(1, String::from("-o"))
        }

        if self.verify {
            arguments.insert(1, String::from("-v"))
        }

        if self.recursive {
            arguments.insert(1, String::from("-R"))
        }

        if self.force {
            arguments.insert(1, String::from("-f"))
        }

        if self.directory {
            arguments.insert(1, String::from("-d"))
        }

        arguments.join(" ")
    }

    /// Calculates the send rate in packets per second
    pub(crate) fn pps(&self) -> u64 {
        self.rate.0 / PACKET_SIZE as u64
    }
}

#[derive(Clone, PartialEq)]
pub(crate) enum Mode {
    Local,
    Remote(bool), // Remote(sender)
    Controller,   // two remotes
}

impl FromStr for Mode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "l" => Self::Local,
            "rr" => Self::Remote(false),
            "rs" => Self::Remote(true),
            "c" => Self::Controller,
            _ => return Err(Self::Err::unknown_mode()),
        })
    }
}

#[derive(Copy, Clone)]
pub(crate) enum SetupMode {
    Listen,
    Connect,
}

impl FromStr for SetupMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "l" => Self::Listen,
            "c" => Self::Connect,
            _ => return Err(Self::Err::unknown_mode()),
        })
    }
}

impl Display for SetupMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Listen => "l",
                Self::Connect => "c",
            }
        )
    }
}

impl Not for SetupMode {
    type Output = Self;

    fn not(self) -> Self::Output {
        match self {
            Self::Listen => Self::Connect,
            Self::Connect => Self::Listen,
        }
    }
}

impl FromStr for Crypto {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut captures = s.split(':');

        // unwrap is safe because there will always be at least one capture
        let cipher_str = captures.next().unwrap().to_uppercase();
        let cipher = Cipher::from_str_name(&cipher_str).ok_or(Self::Err::invalid_cipher())?;

        let key = captures
            .next()
            .map(|m| STANDARD_NO_PAD.decode(m))
            .transpose()? // propagate the decode error
            .unwrap_or_else(|| random_bytes(cipher.key_length()));

        let iv = captures
            .next()
            .map(|m| STANDARD_NO_PAD.decode(m))
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
        // encode the key & iv to base64
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
#[derive(Clone)]
pub(crate) struct IoSpec {
    pub(crate) file_path: PathBuf,
    pub(crate) host: Option<SocketAddr>,
    pub(crate) username: String,
}

impl FromStr for IoSpec {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let captures = Regex::new("(?:(?:([\\w-]+)@)?([\\w.-]+)(?::(\\d+))?:)?(.+)")
            .unwrap() // infallible
            .captures(s)
            .ok_or(Self::Err::invalid_io_spec())?;

        let username = captures
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or(whoami::username());

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
                        (host, port)
                            .to_socket_addrs()?
                            .next() // use the first address
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

        match self.host {
            None => write!(f, "{}@localhost:{}", self.username, file_path),
            Some(host) => write!(f, "{}@{}:{}", self.username, host, file_path),
        }
    }
}

impl IoSpec {
    pub(crate) fn is_local(&self) -> bool {
        match self.host {
            None => true,
            Some(host) => host.ip().is_loopback(),
        }
    }
}

#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
}

#[derive(Debug)]
enum ErrorKind {
    Io(io::Error),
    Decode(base64::DecodeError),
    InvalidIoSpec,
    UnknownMode,
    InvalidCipher,
    InvalidKey,
    InvalidIv,
    NoSuchHost,
    InvalidPort,
}

#[allow(deprecated)]
impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match &self.kind {
                ErrorKind::Io(error) => error::Error::description(error),
                ErrorKind::Decode(error) => error::Error::description(error),
                ErrorKind::InvalidIoSpec => "invalid IoSpec, refer to --help for more information",
                ErrorKind::UnknownMode => "the mode can be either sender or receiver",
                ErrorKind::InvalidCipher => "invalid cipher",
                ErrorKind::InvalidKey => "invalid key",
                ErrorKind::InvalidIv => "invalid IV",
                ErrorKind::NoSuchHost => "no such host",
                ErrorKind::InvalidPort => "invalid port",
            }
        )
    }
}

impl error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self {
            kind: ErrorKind::Io(error),
        }
    }
}

impl From<base64::DecodeError> for Error {
    fn from(error: base64::DecodeError) -> Self {
        Self {
            kind: ErrorKind::Decode(error),
        }
    }
}

impl Error {
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

    fn invalid_io_spec() -> Self {
        Self {
            kind: ErrorKind::InvalidIoSpec,
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
