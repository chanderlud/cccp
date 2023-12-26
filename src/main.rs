#![feature(int_roundings)]

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use async_ssh2_tokio::{AuthMethod, Client, ServerCheckMethod};
use blake3::{Hash, Hasher};
use clap::{CommandFactory, Parser};
use futures::stream::iter;
use futures::{StreamExt, TryStreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, error, info, warn};
use rpassword::prompt_password;
use simple_logging::{log_to_file, log_to_stderr};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket};
use tokio::time::{interval, sleep};
use tokio::{io, select};

use crate::cipher::CipherStream;

use crate::options::{Mode, Options};

mod cipher;
mod error;
mod items;
mod options;
mod receiver;
mod sender;

// result alias used throughout
type Result<T> = std::result::Result<T, error::Error>;

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

#[derive(Clone, Default)]
struct TransferStats {
    confirmed_data: Arc<AtomicUsize>,
    total_data: Arc<AtomicUsize>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut options = Options::parse();
    let mut command = Options::command();

    match options.mode {
        Mode::Local => {
            if let Some(path) = &options.log_file {
                log_to_file(path, options.log_level)?
            } else {
                log_to_stderr(options.log_level)
            }
        }
        _ => log_to_file("cccp.log", options.log_level)?,
    }

    // only the local client needs to handle input validation
    if options.mode == Mode::Local {
        if options.start_port > options.end_port {
            command
                .error(
                    clap::error::ErrorKind::ValueValidation,
                    "end port must be greater than start port",
                )
                .exit();
        }

        let port_count = options.end_port - options.start_port + 1;

        if port_count < 3 {
            command
                .error(
                    clap::error::ErrorKind::ValueValidation,
                    "a minimum of three ports are required",
                )
                .exit();
        } else if port_count - 2 < options.threads as u16 {
            warn!(
                "{} ports < {} threads. decreasing threads to {}",
                port_count - 2,
                options.threads,
                port_count - 2
            );
            options.threads = (port_count - 2) as usize;
        } else if port_count - 2 > options.threads as u16 {
            let new_end = options.start_port + options.threads as u16 + 1;

            warn!(
                "{} ports > {} threads. changing to {}-{}",
                port_count - 2,
                options.threads,
                options.start_port,
                new_end
            );

            options.end_port = new_end;
        }

        if options.destination.is_local() && options.source.is_local() {
            command
                .error(
                    clap::error::ErrorKind::ValueValidation,
                    "both the source and destination cannot be local",
                )
                .exit();
        }
    }

    let sender = options.source.is_local();
    let stats = TransferStats::default();

    let result = match options.mode {
        Mode::Local => {
            let (local, remote) = if sender {
                (&options.source, &mut options.destination)
            } else {
                (&options.destination, &mut options.source)
            };

            if remote.host.is_none() {
                command
                    .error(
                        clap::error::ErrorKind::ValueValidation,
                        format!("host must be specified for remote IoSpec: {}", remote),
                    )
                    .exit();
            } else if remote.username.is_none() {
                remote.username = Some(whoami::username());
            }

            debug!("local {}", local);
            debug!("remote {}", remote);

            let mut auth_method = if let Ok(auth) = ssh_key_auth().await {
                auth
            } else {
                password_auth()?
            };

            // unwrap is safe because we check for a host above
            let remote_addr = remote.host.unwrap();
            let remote_ip = remote_addr.ip();

            let client = loop {
                match Client::connect(
                    remote_addr,
                    remote.username.as_ref().unwrap().as_str(), // unwrap is safe because we check for a username above
                    auth_method.clone(),
                    ServerCheckMethod::NoCheck,
                )
                .await
                {
                    Ok(client) => break client,
                    Err(error) => {
                        warn!("failed to connect to remote host: {}", error);

                        match error {
                            async_ssh2_tokio::error::Error::KeyAuthFailed => {
                                info!("trying password auth");
                                auth_method = password_auth().unwrap();
                            }
                            _ => return Err(error.into()),
                        }
                    }
                }
            };

            info!("connected to the remote host via ssh");

            let command_str = options.format_command(sender);

            let command_handle = tokio::spawn(async move {
                info!("executing command on remote host");
                debug!("command: {}", command_str);

                client.execute(&command_str).await
            });

            // receiver -> sender stream
            let stream =
                connect_stream(remote_ip, options.start_port, options.bind_address).await?;
            let rts_stream = CipherStream::new(stream, &options.control_crypto)?;

            // sender -> receiver stream
            let stream =
                connect_stream(remote_ip, options.start_port + 1, options.bind_address).await?;
            let str_stream = CipherStream::new(stream, &options.control_crypto)?;

            let display_handle = tokio::spawn({
                let stats = stats.clone();

                print_progress(stats)
            });

            let main_future = async {
                if sender {
                    sender::main(options, stats, rts_stream, str_stream, remote_ip).await
                } else {
                    receiver::main(options, stats, rts_stream, str_stream, remote_ip).await
                }
            };

            let command_future = async {
                let result = command_handle.await;

                match result {
                    Ok(Ok(result)) => {
                        if result.exit_status != 0 {
                            // return to terminate execution
                            error!("remote command failed: {:?}", result);
                        } else {
                            info!("remote client exited successfully");
                            // wait forever to allow the other futures to complete
                            sleep(Duration::from_secs(u64::MAX)).await;
                        }
                    }
                    Ok(Err(error)) => error!("remote client failed: {}", error), // return to terminate execution
                    Err(error) => error!("failed to join remote command: {}", error), // return to terminate execution
                }
            };

            select! {
                _ = command_future => Ok(()),
                _ = display_handle => Ok(()),
                result = main_future => result
            }
        }
        Mode::Remote(sender) => {
            // receiver -> sender stream
            let listener = TcpListener::bind(("0.0.0.0", options.start_port)).await?;
            let (stream, remote_addr) = listener.accept().await?;

            let rts_stream = CipherStream::new(stream, &options.control_crypto)?;

            // sender -> receiver stream
            let listener = TcpListener::bind(("0.0.0.0", options.start_port + 1)).await?;
            let (stream, _) = listener.accept().await?;

            let str_stream = CipherStream::new(stream, &options.control_crypto)?;

            let remote_addr = remote_addr.ip();

            if sender {
                sender::main(options, stats, rts_stream, str_stream, remote_addr).await
            } else {
                receiver::main(options, stats, rts_stream, str_stream, remote_addr).await
            }
        }
    };

    if let Err(error) = &result {
        error!("{:?}", error);
    }

    info!("exiting");
    result
}

/// opens the sockets that will be used to send data
async fn socket_factory(
    start: u16,
    end: u16,
    remote_addr: IpAddr,
    threads: usize,
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
        .buffer_unordered(threads)
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

async fn hash_file<P: AsRef<Path>>(path: P) -> io::Result<Hash> {
    let file = File::open(path).await?;
    let mut reader = BufReader::with_capacity(READ_BUFFER_SIZE, file);
    let mut buffer = [0; 2048];

    let mut hasher = Hasher::new();

    loop {
        let read = reader.read(&mut buffer).await?;

        if read != 0 {
            hasher.update(&buffer[..read]);
        } else {
            break;
        }
    }

    Ok(hasher.finalize())
}
