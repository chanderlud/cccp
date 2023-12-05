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
use clap::Parser;
use futures::stream::iter;
use futures::{StreamExt, TryStreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, error, info, warn, LevelFilter};
use regex::Regex;
use rpassword::prompt_password;
use simple_logging::{log_to_file, log_to_stderr};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpSocket, UdpSocket};
use tokio::time::{interval, sleep};
use tokio::{io, select};

mod receiver;
mod sender;

// type aliases used throughout
type UnlimitedQueue<T> = Arc<deadqueue::unlimited::Queue<T>>;
type Result<T> = std::result::Result<T, Error>;

const READ_BUFFER_SIZE: usize = 10_000_000;
const WRITE_BUFFER_SIZE: usize = 5_000_000;
const TRANSFER_BUFFER_SIZE: usize = 1024;
const INDEX_SIZE: usize = std::mem::size_of::<u64>();
const MAX_RETRIES: usize = 10;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(5);

// how long to wait for a job to be confirmed before requeuing it
const REQUEUE_INTERVAL: Duration = Duration::from_millis(1_000);

#[derive(Debug)]
struct Error {
    kind: ErrorKind,
}

#[derive(Debug)]
enum ErrorKind {
    IoError(io::Error),
    ParseError(std::net::AddrParseError),
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self {
            kind: ErrorKind::IoError(error),
        }
    }
}

impl From<std::net::AddrParseError> for Error {
    fn from(error: std::net::AddrParseError) -> Self {
        Self {
            kind: ErrorKind::ParseError(error),
        }
    }
}

impl Termination for Error {
    fn report(self) -> ExitCode {
        ExitCode::from(match self.kind {
            ErrorKind::IoError(error) => match error.kind() {
                io::ErrorKind::NotFound => 1,
                _ => 2,
            },
            ErrorKind::ParseError(_) => 3,
        })
    }
}

#[derive(Parser, Clone, Debug)]
struct Options {
    #[clap(
        short,
        long = "mode",
        help = "local or remote",
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
        help = "the rate to send data at (bytes per second)",
        default_value = "10000000"
    )]
    rate: u64,

    #[clap(help = "where to get the data from")]
    source: FileLocation,

    #[clap(help = "where to send the data")]
    destination: FileLocation,
}

impl Options {
    fn format_command(&self, sender: bool) -> String {
        let mode = if sender { "rr" } else { "rs" };

        format!(
            "cccp --mode {} --start-port {} --end-port {} --threads {} --log-level {} --rate {} \"{}\" \"{}\"",
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

    fn is_dir(&self) -> bool {
        self.file_path.is_dir()
    }

    async fn file_size(&self) -> io::Result<u64> {
        let metadata = tokio::fs::metadata(&self.file_path).await?;
        Ok(metadata.len())
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

        let port_count = options.end_port - options.start_port;

        if port_count < options.threads {
            warn!(
                "{} ports < {} threads. decreasing threads to {}",
                port_count, options.threads, port_count
            );
            options.threads = port_count;
        } else if port_count < 2 {
            panic!("a minimum of two ports are required")
        } else if port_count > options.threads {
            warn!(
                "{} ports > {} threads. changing port range to {}-{}",
                port_count,
                options.threads,
                options.start_port,
                options.start_port + options.threads
            );
            options.end_port = options.start_port + options.threads;
        }

        if options.destination.host.is_none() && options.source.host.is_none() {
            panic!("at least one host must be specified")
        }

        // UDP header + INDEX + DATA
        let packet_size = (8 + INDEX_SIZE + TRANSFER_BUFFER_SIZE) as u64;
        let pps_rate = options.rate / packet_size;
        debug!("{} byte/s -> {} packet/s", options.rate, pps_rate);
        options.rate = pps_rate;
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

            let bind = match options.bind_address {
                Some(addr) => SocketAddr::new(addr, 0),
                None => "0.0.0.0:0".parse().unwrap(),
            };

            // connect to the remote client on the first port in the range
            let control_stream = loop {
                let socket = TcpSocket::new_v4().unwrap();
                socket.bind(bind).unwrap();

                let remote_socket = SocketAddr::new(remote_addr, options.start_port);

                if let Ok(stream) = socket.connect(remote_socket).await {
                    break stream;
                } else {
                    // give the receiver time to start listening
                    sleep(Duration::from_millis(100)).await;
                }
            };

            let display_handle = tokio::spawn({
                let stats = stats.clone();

                print_progress(stats)
            });

            let main_future = async {
                if sender {
                    sender::main(options, stats, control_stream, remote_addr).await
                } else {
                    receiver::main(options, stats, control_stream, remote_addr).await
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
            let listener = TcpListener::bind(("0.0.0.0", options.start_port)).await?;
            let (control_stream, remote_addr) = listener.accept().await?;

            if sender {
                sender::main(options, stats, control_stream, remote_addr.ip()).await?;
            } else {
                receiver::main(options, stats, control_stream, remote_addr.ip()).await?;
            };
        }
    }

    info!("exiting");
    Ok(())
}

// opens the sockets that will be used to send data
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

fn password_auth() -> io::Result<AuthMethod> {
    let password = prompt_password("password: ")?;
    Ok(AuthMethod::with_password(&password))
}

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
