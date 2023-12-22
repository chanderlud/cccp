#![feature(int_roundings)]

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use aes::Aes256;
use async_ssh2_tokio::{AuthMethod, Client, ServerCheckMethod};
use blake3::{Hash, Hasher};
use chacha20::cipher::generic_array::GenericArray;
use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use chacha20::{ChaCha20, ChaCha8};
use clap::{CommandFactory, Parser};
use ctr::Ctr128BE;
use futures::stream::iter;
use futures::{StreamExt, TryStreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, error, info, warn};
use prost::Message;
use rpassword::prompt_password;
use simple_logging::{log_to_file, log_to_stderr};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket};
use tokio::time::{interval, sleep};
use tokio::{io, select};

use crate::items::{Cipher, Crypto};
use crate::options::{Mode, Options};

mod error;
mod items;
mod options;
mod receiver;
mod sender;

// result alias used throughout
type Result<T> = std::result::Result<T, error::Error>;

trait StreamCipherExt: Send + Sync {
    fn seek(&mut self, index: u64);
    fn apply_keystream(&mut self, data: &mut [u8]);
}

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

impl StreamCipherExt for ChaCha20 {
    fn seek(&mut self, index: u64) {
        StreamCipherSeek::seek(self, index);
    }

    fn apply_keystream(&mut self, buf: &mut [u8]) {
        StreamCipher::apply_keystream(self, buf)
    }
}

impl StreamCipherExt for ChaCha8 {
    fn seek(&mut self, index: u64) {
        StreamCipherSeek::seek(self, index);
    }

    fn apply_keystream(&mut self, buf: &mut [u8]) {
        StreamCipher::apply_keystream(self, buf)
    }
}

impl StreamCipherExt for Ctr128BE<Aes256> {
    fn seek(&mut self, index: u64) {
        StreamCipherSeek::seek(self, index);
    }

    fn apply_keystream(&mut self, buf: &mut [u8]) {
        StreamCipher::apply_keystream(self, buf)
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
    let mut command = Options::command();

    match options.mode {
        Mode::Local => log_to_stderr(options.log_level),
        _ => log_to_file("cccp.log", options.log_level).expect("failed to log"),
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
            command
                .error(
                    clap::error::ErrorKind::ValueValidation,
                    "either the source or destination must be remote",
                )
                .exit();
        } else if options.destination.is_local() && options.source.is_local() {
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
            let command_str = options.format_command(sender);

            let (local, remote) = if sender {
                (&options.source, &mut options.destination)
            } else {
                (&options.destination, &mut options.source)
            };

            if remote.username.is_none() {
                warn!(
                    "username not specified for remote host (trying \"\"): {}",
                    remote
                );
                remote.username = Some(String::new());
            } else if remote.host.is_none() {
                command
                    .error(
                        clap::error::ErrorKind::ValueValidation,
                        format!("host must be specified for remote host: {}", remote),
                    )
                    .exit();
            }

            debug!("local {}", local);
            debug!("remote {}", remote);

            let mut auth_method = ssh_key_auth().await.unwrap_or_else(|error| {
                warn!("failed to use ssh key auth: {}", error);
                password_auth().unwrap()
            });

            // unwrap is safe because we check for a host above
            let remote_addr = remote.host.unwrap();

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

            let command_handle = tokio::spawn(async move {
                info!("executing command on remote host");
                debug!("command: {}", command_str);

                client.execute(&command_str).await
            });

            // receiver -> sender stream
            let rts_stream =
                connect_stream(remote_addr.ip(), options.start_port, options.bind_address).await?;
            // sender -> receiver stream
            let str_stream = connect_stream(
                remote_addr.ip(),
                options.start_port + 1,
                options.bind_address,
            )
            .await?;

            let display_handle = tokio::spawn({
                let stats = stats.clone();

                print_progress(stats)
            });

            let main_future = async {
                if sender {
                    sender::main(
                        options,
                        stats.clone(),
                        rts_stream,
                        str_stream,
                        remote_addr.ip(),
                    )
                    .await
                } else {
                    receiver::main(
                        options,
                        stats.clone(),
                        rts_stream,
                        str_stream,
                        remote_addr.ip(),
                    )
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
                _ = command_future => Ok(()),
                _ = display_handle => Ok(()),
                result = main_future => result
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
async fn write_message<W: AsyncWrite + Unpin, M: Message, C: StreamCipherExt + ?Sized>(
    writer: &mut W,
    message: &M,
    cipher: &mut Box<C>,
) -> Result<()> {
    let len = message.encoded_len(); // get the length of the message
    writer.write_u32(len as u32).await?; // write the length of the message

    let mut buffer = Vec::with_capacity(len); // create a buffer to write the message into
    message.encode(&mut buffer).unwrap(); // encode the message into the buffer (infallible)
    cipher.apply_keystream(&mut buffer[..]); // encrypt the message

    writer.write_all(&buffer).await?; // write the message to the writer

    Ok(())
}

/// read a `Message` from a reader
async fn read_message<
    R: AsyncReadExt + Unpin,
    M: Message + Default,
    C: StreamCipherExt + ?Sized,
>(
    reader: &mut R,
    cipher: &mut Box<C>,
) -> Result<M> {
    let len = reader.read_u32().await? as usize; // read the length of the message

    let mut buffer = vec![0; len]; // create a buffer to read the message into
    reader.read_exact(&mut buffer).await?; // read the message into the buffer
    cipher.apply_keystream(&mut buffer[..]); // decrypt the message

    let message = M::decode(&buffer[..])?; // decode the message

    Ok(message)
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

fn make_cipher(crypto: &Crypto) -> Box<dyn StreamCipherExt> {
    let key = GenericArray::from_slice(&crypto.key[..32]);

    match crypto.cipher.try_into() {
        Ok(Cipher::Aes) => {
            let iv = GenericArray::from_slice(&crypto.key[..16]);

            Box::new(Ctr128BE::<Aes256>::new(key, iv))
        }
        Ok(Cipher::Chacha8) => {
            let iv = GenericArray::from_slice(&crypto.key[..12]);

            Box::new(ChaCha8::new(key, iv))
        }
        Ok(Cipher::Chacha20) => {
            let iv = GenericArray::from_slice(&crypto.key[..12]);

            Box::new(ChaCha20::new(key, iv))
        }
        _ => unreachable!(),
    }
}
