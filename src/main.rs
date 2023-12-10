#![feature(int_roundings)]

use std::error;
use std::fmt::{Display, Formatter};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::{ExitCode, Termination};
use std::str::FromStr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use async_ssh2_tokio::{AuthMethod, Client, ServerCheckMethod};
use bytesize::ByteSize;
use clap::Parser;
use futures::stream::iter;
use futures::{StreamExt, TryStreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use kanal::{AsyncReceiver, ReceiveError, SendError};
use log::{debug, error, info, warn, LevelFilter};
use prost::Message;
use regex::Regex;
use rpassword::prompt_password;
use simple_logging::{log_to_file, log_to_stderr};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket};
use tokio::time::{interval, sleep};
use tokio::{io, select};

mod receiver;
mod sender;

// result alias used throughout
type Result<T> = std::result::Result<T, Error>;

// read buffer must be a multiple of the transfer buffer to prevent a nasty little bug
const READ_BUFFER_SIZE: usize = TRANSFER_BUFFER_SIZE * 100;
const WRITE_BUFFER_SIZE: usize = TRANSFER_BUFFER_SIZE * 100;
const TRANSFER_BUFFER_SIZE: usize = 1024;
const INDEX_SIZE: usize = std::mem::size_of::<u64>();
const ID_SIZE: usize = std::mem::size_of::<u32>();
const MAX_RETRIES: usize = 10;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(5);
// UDP header + ID + INDEX + DATA
const PACKET_SIZE: usize = 8 + ID_SIZE + INDEX_SIZE + TRANSFER_BUFFER_SIZE;

// how long to wait for a job to be confirmed before requeuing it
const REQUEUE_INTERVAL: Duration = Duration::from_millis(1_000);

pub mod items {
    include!(concat!(env!("OUT_DIR"), "/cccp.items.rs"));
}

#[derive(Debug)]
struct Error {
    kind: ErrorKind,
}

#[derive(Debug)]
enum ErrorKind {
    Io(io::Error),
    Parse(std::net::AddrParseError),
    Decode(prost::DecodeError),
    Join(tokio::task::JoinError),
    Send(SendError),
    Receive(ReceiveError),
    MissingQueue,
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
            ErrorKind::MissingQueue => 7,
        })
    }
}

impl Error {
    fn missing_queue() -> Self {
        Self {
            kind: ErrorKind::MissingQueue,
        }
    }
}

#[derive(Parser, Clone, Debug)]
struct Options {
    #[clap(
        long = "mode",
        hide = true, // the user does not need to set this
        default_value = "local"
    )]
    mode: Mode,

    #[clap(
        short,
        long = "start-port",
        help = "the first port to use",
        default_value = "50000"
    )]
    start_port: u16,

    #[clap(
        short,
        long = "end-port",
        help = "the last port to use",
        default_value = "50100"
    )]
    end_port: u16,

    #[clap(
        short,
        long = "threads",
        help = "how many threads to use",
        default_value = "100"
    )]
    threads: u16,

    #[clap(
        short,
        long = "log-level",
        help = "log level [debug, info, warn, error]",
        default_value = "warn"
    )]
    log_level: LevelFilter,

    #[clap(
        short,
        long = "bind-address",
        help = "manually specify the address to listen on"
    )]
    bind_address: Option<IpAddr>,

    #[clap(
        short,
        long = "rate",
        help = "the rate to send data at [b, kb, mb, gb, tb]",
        default_value = "1mb"
    )]
    rate: ByteSize,

    #[clap(
        short,
        long = "max",
        help = "the maximum number of concurrent transfers",
        default_value = "100"
    )]
    max: usize,

    #[clap(help = "where to get the data from")]
    source: FileLocation,

    #[clap(help = "where to send the data")]
    destination: FileLocation,
}

impl Options {
    fn format_command(&self, sender: bool) -> String {
        let mode = if sender { "rr" } else { "rs" };

        format!(
            "cccp --mode {} --start-port {} --end-port {} --threads {} --log-level {} --rate \"{}\" \"{}\" \"{}\"",
            mode,
            self.start_port,
            self.end_port,
            self.threads,
            self.log_level,
            self.rate,
            self.source,
            self.destination
        )
    }

    fn pps(&self) -> u64 {
        self.rate.0 / PACKET_SIZE as u64
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Mode {
    Local,
    Remote(bool), // Remote(sender)
}

impl FromStr for Mode {
    type Err = CustomParseErrors;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "l" => Self::Local,
            "rr" => Self::Remote(false),
            "rs" => Self::Remote(true),
            "local" => Self::Local,
            "remote-receiver" => Self::Remote(false),
            "remote-sender" => Self::Remote(true),
            _ => return Err(CustomParseErrors::UnknownMode),
        })
    }
}

// a file located anywhere
#[derive(Clone, Debug)]
struct FileLocation {
    file_path: PathBuf,
    host: Option<String>,
    username: Option<String>,
}

impl FromStr for FileLocation {
    type Err = CustomParseErrors;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let (username, host, file_path_str) = if s.contains('@') {
            let captures = Regex::new("([^:]+)@([^:]+):(.+)")
                .unwrap()
                .captures(s)
                .ok_or(CustomParseErrors::MalformedConnectionString(
                    "input does not match format",
                ))?;
            (
                captures.get(1).map(|m| m.as_str().to_string()),
                captures.get(2).map(|m| m.as_str().to_string()),
                captures.get(3).map(|m| m.as_str().to_string()),
            )
        } else if s.contains(':') {
            let (host, file_path_str) =
                s.split_once(':')
                    .ok_or(CustomParseErrors::MalformedConnectionString(
                        "input does not match known format",
                    ))?;
            (
                None,
                Some(host.to_string()),
                Some(file_path_str.to_string()),
            )
        } else {
            (None, None, Some(s.to_string()))
        };

        let file_path = file_path_str
            .ok_or(CustomParseErrors::MalformedConnectionString(
                "missing file path",
            ))?
            .parse()
            .map_err(|_| CustomParseErrors::ParseError)?;

        Ok(Self {
            file_path,
            host,
            username,
        })
    }
}

impl Display for FileLocation {
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

impl FileLocation {
    fn is_local(&self) -> bool {
        self.host.is_none() || (self.host.is_some() && self.file_path.exists())
    }
}

#[derive(Clone, Debug)]
enum CustomParseErrors {
    MalformedConnectionString(&'static str),
    UnknownMode,
    ParseError,
    IoError,
}

impl Display for CustomParseErrors {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::MalformedConnectionString(message) => message,
                Self::UnknownMode => "The mode can be either sender or receiver",
                Self::ParseError => "Invalid file path",
                Self::IoError => "An error occurred while getting the size of a file",
            }
        )
    }
}

impl error::Error for CustomParseErrors {}

impl From<io::Error> for CustomParseErrors {
    fn from(_: io::Error) -> Self {
        Self::IoError
    }
}

#[derive(Clone, Default)]
struct TransferStats {
    confirmed_data: Arc<AtomicUsize>,
    total_data: Arc<AtomicUsize>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut options = Options::parse();

    match options.mode {
        Mode::Local => log_to_stderr(options.log_level),
        _ => log_to_file("cccp.log", options.log_level).expect("failed to log"),
    }

    // only the local client needs to handle input validation
    if options.mode == Mode::Local {
        if options.start_port > options.end_port {
            panic!("end port must be greater than start port")
        }

        let port_count = options.end_port - options.start_port + 1;

        if port_count < 3 {
            panic!("a minimum of three ports are required")
        } else if port_count - 2 < options.threads {
            warn!(
                "{} ports < {} threads. decreasing threads to {}",
                port_count - 2,
                options.threads,
                port_count - 2
            );
            options.threads = port_count - 2;
        } else if port_count - 2 > options.threads {
            let new_end = options.start_port + options.threads + 1;

            warn!(
                "{} ports > {} threads. changing to {}-{}",
                port_count - 2,
                options.threads,
                options.start_port,
                new_end
            );

            options.end_port = new_end;
        }

        if options.destination.host.is_none() && options.source.host.is_none() {
            panic!("either the source or destination must be remote");
        }
    }

    let sender = options.source.is_local();
    let stats = TransferStats::default();

    match options.mode {
        Mode::Local => {
            let command = options.format_command(sender);

            let (local, remote) = if sender {
                (&options.source, &options.destination)
            } else {
                (&options.destination, &options.source)
            };

            if remote.username.is_none() {
                panic!("username must be specified for remote host: {}", remote);
            } else if remote.host.is_none() {
                panic!("host must be specified for remote host: {}", remote);
            }

            debug!("local {}", local);
            debug!("remote {}", remote);

            let mut auth_method = match ssh_key_auth().await {
                Ok(auth) => auth,
                Err(error) => {
                    warn!("failed to use ssh key auth: {}", error);
                    password_auth().unwrap()
                }
            };

            let remote_addr: IpAddr = remote.host.as_ref().unwrap().parse().unwrap();

            let client = loop {
                match Client::connect(
                    (remote_addr, 22),
                    remote.username.as_ref().unwrap().as_str(),
                    auth_method.clone(),
                    ServerCheckMethod::NoCheck,
                )
                .await
                {
                    Ok(client) => break client,
                    Err(error) => {
                        warn!("failed to connect to local host: {}", error);

                        match error {
                            async_ssh2_tokio::error::Error::KeyAuthFailed => {
                                info!("trying password auth");
                                auth_method = password_auth().unwrap();
                            }
                            _ => panic!("failed to connect to remote host: {}", error),
                        }
                    }
                }
            };

            info!("connected to the remote host via ssh");

            let command_handle = tokio::spawn(async move {
                info!("executing command on remote host");
                debug!("command: {}", command);

                client.execute(&command).await
            });

            // receiver -> sender stream
            let rts_stream =
                connect_stream(remote_addr, options.start_port, options.bind_address).await?;
            // sender -> receiver stream
            let str_stream =
                connect_stream(remote_addr, options.start_port + 1, options.bind_address).await?;

            let display_handle = tokio::spawn({
                let stats = stats.clone();

                print_progress(stats)
            });

            let main_future = async {
                if sender {
                    sender::main(options, stats.clone(), rts_stream, str_stream, remote_addr).await
                } else {
                    receiver::main(options, stats.clone(), rts_stream, str_stream, remote_addr)
                        .await
                }
            };

            let command_future = async {
                let result = command_handle.await;

                match result {
                    Ok(Ok(result)) => {
                        match result.exit_status {
                            0 => {
                                info!("remote client exited successfully");
                                // wait forever to allow the other futures to complete
                                sleep(Duration::from_secs(u64::MAX)).await;
                            }
                            1 => error!("remote client failed, file not found"),
                            2 => error!("remote client failed, unknown IO error"),
                            3 => error!("remote client failed, parse error"),
                            4 => error!("remote client failed, decode error"),
                            5 => error!("remote client failed, join error"),
                            _ => error!("remote client failed, unknown error"),
                        }
                    }
                    Ok(Err(error)) => error!("remote client failed: {}", error), // return to terminate execution
                    Err(error) => error!("failed to join remote command: {}", error), // return to terminate execution
                }
            };

            select! {
                _ = command_future => {},
                _ = display_handle => {},
                result = main_future => result?
            }
        }
        Mode::Remote(sender) => {
            // receiver -> sender stream
            let listener = TcpListener::bind(("0.0.0.0", options.start_port)).await?;
            let (rts_stream, remote_addr) = listener.accept().await?;

            // sender -> receiver stream
            let listener = TcpListener::bind(("0.0.0.0", options.start_port + 1)).await?;
            let (str_stream, _) = listener.accept().await?;

            let remote_addr = remote_addr.ip();

            if sender {
                sender::main(options, stats, rts_stream, str_stream, remote_addr).await?;
            } else {
                receiver::main(options, stats, rts_stream, str_stream, remote_addr).await?;
            };
        }
    }

    info!("exiting");
    Ok(())
}

/// opens the sockets that will be used to send data
async fn socket_factory(
    start: u16,
    end: u16,
    remote_addr: IpAddr,
    threads: u16,
) -> io::Result<Vec<UdpSocket>> {
    let bind_addr: IpAddr = "0.0.0.0".parse().unwrap();

    iter(start..=end)
        .map(|port| {
            let local_addr = SocketAddr::new(bind_addr, port);
            let remote_addr = SocketAddr::new(remote_addr, port);

            async move {
                let socket = UdpSocket::bind(local_addr).await?;
                socket.connect(remote_addr).await?;
                Ok::<UdpSocket, io::Error>(socket)
            }
        })
        .buffer_unordered(threads as usize)
        .try_collect()
        .await
}

/// try to get an ssh key for authentication
async fn ssh_key_auth() -> io::Result<AuthMethod> {
    // get the home directory of the current user
    let home_dir = dirs::home_dir().ok_or(io::Error::new(
        io::ErrorKind::NotFound,
        "home directory not found",
    ))?;

    // append the `.ssh/id_ed25519` path to the home directory
    let key_path = home_dir.join(".ssh").join("id_ed25519");

    // open the SSH private key file
    let mut key_file = File::open(key_path).await?;

    // read the contents of the key file into a string
    let mut key = String::new();
    key_file.read_to_string(&mut key).await?;

    Ok(AuthMethod::with_key(&key, None))
}

/// prompt the user for a password
fn password_auth() -> io::Result<AuthMethod> {
    let password = prompt_password("password: ")?;
    Ok(AuthMethod::with_password(&password))
}

/// print a progress bar to stdout
async fn print_progress(stats: TransferStats) {
    let bar = ProgressBar::new(100);
    let mut interval = interval(Duration::from_secs(1));

    bar.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos:>7}% ({eta})")
            .unwrap()
            .progress_chars("=>-"),
    );

    loop {
        interval.tick().await;
        let progress =
            stats.confirmed_data.load(Relaxed) as f64 / stats.total_data.load(Relaxed) as f64;
        bar.set_position((progress * 100_f64) as u64);
    }
}

/// connect to a remote client on a given port
async fn connect_stream(
    remote_addr: IpAddr,
    port: u16,
    bind_addr: Option<IpAddr>,
) -> Result<TcpStream> {
    let bind = match bind_addr {
        Some(addr) => SocketAddr::new(addr, 0),
        None => "0.0.0.0:0".parse()?,
    };

    // connect to the remote client
    loop {
        let socket = TcpSocket::new_v4()?;
        socket.bind(bind)?;

        let remote_socket = SocketAddr::new(remote_addr, port);

        if let Ok(stream) = socket.connect(remote_socket).await {
            break Ok(stream);
        } else {
            // give the receiver time to start listening
            sleep(Duration::from_millis(100)).await;
        }
    }
}

/// write a `Message` to a writer
async fn write_message<W: AsyncWrite + Unpin, M: Message>(
    writer: &mut W,
    message: &M,
) -> Result<()> {
    let len = message.encoded_len();
    writer.write_u32(len as u32).await?;

    let mut buffer = Vec::with_capacity(len);
    message.encode(&mut buffer).unwrap();

    writer.write_all(&buffer).await?;

    Ok(())
}

/// read a `Message` from a reader
async fn read_message<R: AsyncReadExt + Unpin, M: Message + Default>(reader: &mut R) -> Result<M> {
    let len = reader.read_u32().await? as usize;

    let mut buffer = vec![0; len];
    reader.read_exact(&mut buffer).await?;

    let message = M::decode(&buffer[..])?;

    Ok(message)
}

/// send messages from a channel to a writer
async fn message_sender<W: AsyncWrite + Unpin, M: Message>(
    mut writer: W,
    receiver: AsyncReceiver<M>,
) -> Result<()> {
    while let Ok(message) = receiver.recv().await {
        write_message(&mut writer, &message).await?;
    }

    Ok(())
}
