use std::fmt::{Display, Formatter};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;
use std::{error, process};

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
use tokio::net::UdpSocket;
use tokio::time::{interval, sleep};
use tokio::{io, select};

mod receiver;
mod sender;

type UnlimitedQueue<T> = Arc<deadqueue::unlimited::Queue<T>>;
type LimitedQueue<T> = Arc<deadqueue::limited::Queue<T>>;
type Result<T> = std::result::Result<T, Error>;

const READ_BUFFER_SIZE: usize = 10_000_000;
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

impl Error {
    fn exit_code(self) -> i32 {
        match self.kind {
            ErrorKind::IoError(error) => match error.kind() {
                io::ErrorKind::NotFound => 1,
                _ => 2,
            },
            ErrorKind::ParseError(_) => 3,
        }
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
        default_value = "info"
    )]
    log_level: LevelFilter,

    #[clap(
        short,
        long = "bind-address",
        help = "manually specify the address to listen on"
    )]
    bind_address: Option<String>,

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
    RemoteSender,
    RemoteReceiver,
}

impl FromStr for Mode {
    type Err = CustomParseErrors;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "l" => Self::Local,
            "rr" => Self::RemoteReceiver,
            "rs" => Self::RemoteSender,
            "local" => Self::Local,
            "remote-receiver" => Self::RemoteReceiver,
            "remote-sender" => Self::RemoteSender,
            _ => return Err(CustomParseErrors::UnknownMode),
        })
    }
}

// a file located anywhere:tm:
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
    fn is_local(&self, local_address: &str) -> bool {
        self.host.is_none() || self.host.as_ref().unwrap() == local_address
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
async fn main() {
    let mut options = Options::parse();

    match options.mode {
        Mode::Local => log_to_stderr(options.log_level),
        // TODO choose a better log file location
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
        }

        if options.destination.host.is_none() && options.source.host.is_none() {
            panic!("at least one host must be specified")
        }

        // UDP header + INDEX + DATA
        let packet_size = (8 + INDEX_SIZE + TRANSFER_BUFFER_SIZE) as u64;
        let pps_rate = options.rate / packet_size;
        info!(
            "converted {} byte/s rate to {} packet/s rate",
            options.rate, pps_rate
        );
        options.rate = pps_rate;
    }

    let public_address = match options.mode {
        Mode::Local => {
            if let Some(address) = options.bind_address.as_ref() {
                address.clone()
            } else {
                reqwest::get("http://api.ipify.org")
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap()
            }
        }
        Mode::RemoteSender => options.source.host.as_ref().unwrap().clone(),
        Mode::RemoteReceiver => options.destination.host.as_ref().unwrap().clone(),
    };

    info!("the bind address is: {}", public_address);

    let sender = options.source.is_local(&public_address);
    let stats = TransferStats::default();

    match options.mode {
        Mode::Local => {
            if options.destination.is_local(&public_address) {
                debug!("destination is local");
                options.destination.host = Some(public_address.clone());
            } else {
                debug!("source is local");
                options.source.host = Some(public_address.clone());
            }

            let command = options.format_command(sender);

            let (local, remote) = if options.destination.is_local(&public_address) {
                (&options.destination, &options.source)
            } else {
                (&options.source, &options.destination)
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

            let client = loop {
                match Client::connect(
                    (remote.host.as_ref().unwrap().as_str(), 22),
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

            let display_handle = tokio::spawn({
                let stats = stats.clone();

                print_progress(stats)
            });

            let main_future = async {
                if sender {
                    sender::main(options, stats).await
                } else {
                    receiver::main(options, stats).await
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
                result = main_future => {
                    if let Err(error) = result {
                        error!("{} failed: {:?}", if sender { "sender" } else { "receiver" }, error);
                    }
                }
            }
        }
        Mode::RemoteSender => {
            if let Err(error) = sender::main(options, stats).await {
                error!("sender failed: {:?}", error);
                process::exit(error.exit_code()); // exit with non 0 status so remote knows it failed
            }
        }
        Mode::RemoteReceiver => {
            if let Err(error) = receiver::main(options, stats).await {
                error!("receiver failed: {:?}", error);
                process::exit(error.exit_code()); // exit with non 0 status so remote knows it failed
            }
        }
    }

    info!("exiting");
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
