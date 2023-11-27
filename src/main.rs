use std::error::Error;
use std::fmt::{Display, Formatter};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use async_ssh2_tokio::{AuthMethod, Client, ServerCheckMethod};
use clap::Parser;
use deadqueue::limited::Queue;
use futures::stream::iter;
use futures::{StreamExt, TryStreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, error, info, warn, LevelFilter};
use regex::Regex;
use rpassword::prompt_password;
use tokio::fs::File;
use tokio::io;
use tokio::io::AsyncReadExt;
use tokio::net::UdpSocket;
use tokio::time::{interval, sleep};

mod receiver;
mod sender;

type ByteQueue = Arc<Queue<Vec<u8>>>;

const READ_BUFFER_SIZE: usize = 10_000_000;
const WRITE_BUFFER_SIZE: usize = 10_000_000;
const TRANSFER_BUFFER_SIZE: usize = 1024;
const INDEX_SIZE: usize = std::mem::size_of::<u64>();

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

        let escaped_source = self.source.to_string().replace(' ', "\\ ");
        let escaped_destination = self.destination.to_string().replace(' ', "\\ ");

        format!(
            "cccp --mode {} --start-port {} --end-port {} --threads {} --log-level {} {} {}",
            mode,
            self.start_port,
            self.end_port,
            self.threads,
            self.log_level,
            escaped_source,
            escaped_destination
        )
    }
}

#[derive(Debug, Clone)]
enum Mode {
    Local,
    RemoteSender,
    RemoteReceiver,
}

impl FromStr for Mode {
    type Err = CustomParseErrors;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
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

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains('@') {
            let regex = Regex::new("([^:]+)@([^:]+):(.+)").unwrap();

            let captures =
                regex
                    .captures(s)
                    .ok_or(CustomParseErrors::MalformedConnectionString(
                        "input does not match format",
                    ))?;

            let username = captures
                .get(1)
                .ok_or(CustomParseErrors::MalformedConnectionString(
                    "missing username",
                ))?;
            let host = captures
                .get(2)
                .ok_or(CustomParseErrors::MalformedConnectionString("missing host"))?;
            let file_path = captures
                .get(3)
                .ok_or(CustomParseErrors::MalformedConnectionString(
                    "missing file path",
                ))?;

            let file_path = file_path
                .as_str()
                .parse()
                .map_err(|_| CustomParseErrors::ParseError)?;

            Ok(Self {
                file_path,
                host: Some(host.as_str().to_string()),
                username: Some(username.as_str().to_string()),
            })
        } else if s.contains(':') {
            let (host, file_path_str) =
                s.split_once(':')
                    .ok_or(CustomParseErrors::MalformedConnectionString(
                        "input does not match known format",
                    ))?;

            let file_path = file_path_str
                .parse()
                .map_err(|_| CustomParseErrors::ParseError)?;

            Ok(Self {
                file_path,
                host: Some(host.to_string()),
                username: None,
            })
        } else {
            let file_path = s.parse().map_err(|_| CustomParseErrors::ParseError)?;

            Ok(Self {
                file_path,
                host: None,
                username: None,
            })
        }
    }
}

impl Display for FileLocation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.username.is_none() && self.host.is_none() {
            write!(f, "{}", self.file_path.display())
        } else if self.username.is_none() {
            write!(
                f,
                "{}:{}",
                self.host.as_ref().unwrap(),
                self.file_path.display()
            )
        } else {
            write!(
                f,
                "{}@{}:{}",
                self.username.as_ref().unwrap(),
                self.host.as_ref().unwrap(),
                self.file_path.display()
            )
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

impl Error for CustomParseErrors {}

impl From<std::io::Error> for CustomParseErrors {
    fn from(_: std::io::Error) -> Self {
        Self::IoError
    }
}

#[derive(Clone, Default)]
struct TransferStats {
    confirmed_data: Arc<AtomicUsize>,
    total_data: Arc<AtomicUsize>
}

#[tokio::main]
async fn main() {
    let mut options = Options::parse();

    // TODO choose a better log file location
    simple_logging::log_to_file("cccp.log", options.log_level).expect("failed to log");

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
    }

    if options.destination.host.is_none() && options.source.host.is_none() {
        panic!("at least one host must be specified")
    }

    let public_address;

    match options.mode {
        Mode::Local => {
            // only the local client needs to handle the rate
            // UDP header + INDEX + DATA
            let packet_size = (8 + INDEX_SIZE + TRANSFER_BUFFER_SIZE) as u64;
            let pps_rate = options.rate / packet_size;
            info!("converted {} byte/s rate to {} packet/s rate", options.rate, pps_rate);
            options.rate = pps_rate;

            if let Some(address) = options.bind_address.as_ref() {
                public_address = address.clone();
            } else {
                public_address = reqwest::get("http://api.ipify.org")
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap();
            }
        }
        Mode::RemoteSender => {
            public_address = options.source.host.as_ref().unwrap().clone();
        }
        Mode::RemoteReceiver => {
            public_address = options.destination.host.as_ref().unwrap().clone();
        }
    }

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

            debug!("local {}", local);
            debug!("remote {}", remote);

            let mut auth_method = match ssh_key_auth().await {
                Ok(auth) => auth,
                Err(error) => {
                    warn!("failed to use ssh key auth: {}", error);
                    password_auth().unwrap()
                },
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

                let result = client
                    .execute(&command)
                    .await
                    .expect("failed to send command");

                if result.exit_status != 0 {
                    warn!("non 0 receiver exit status: {:?}", result)
                }
            });

            // we sleep here to give the remote client time to start
            sleep(Duration::from_secs(1)).await;

            tokio::spawn({
                let stats = stats.clone();

                print_progress(stats)
            });

            if sender {
                debug!("running sender");

                if let Err(error) = sender::main(options, stats).await {
                    error!("sender failed: {:?}", error);
                }
            } else {
                debug!("running receiver");

                if let Err(error) = receiver::main(options, stats).await {
                    error!("receiver failed: {:?}", error);
                }
            }

            // the remote command should have exited now
            command_handle.await.expect("failed to join command");
        }
        Mode::RemoteSender => {
            debug!("running sender");

            if let Err(error) = sender::main(options, stats).await {
                error!("sender failed: {:?}", error);
            }
        }
        Mode::RemoteReceiver => {
            debug!("running receiver");

            if let Err(error) = receiver::main(options, stats).await {
                error!("receiver failed: {:?}", error);
            }
        }
    }
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

    bar.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos:>7}/{len:7} ({eta})")
        .unwrap()
        .progress_chars("=>-"));

    loop {
        interval.tick().await;
        let progress = stats.confirmed_data.load(Relaxed) as f64 / stats.total_data.load(Relaxed) as f64;
        bar.set_position((progress * 100_f64) as u64);
    }
}