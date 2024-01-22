#![feature(int_roundings)]

#[cfg(feature = "installer")]
use std::env;
use std::io::{stdin, BufRead};
use std::net::{IpAddr, SocketAddr};
use std::ops::Not;
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Arc;
use std::time::Duration;

use async_ssh2_tokio::{AuthMethod, Client, ServerCheckMethod};
use blake3::{Hash, Hasher};
use clap::{CommandFactory, Parser};
use futures::stream::iter;
use futures::{StreamExt, TryStreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, error, info, warn};
use prost::Message;
use rpassword::prompt_password;
use russh::ChannelMsg;
use simple_logging::{log_to_file, log_to_stderr};
use tokio::fs::File;
use tokio::io::{stdout, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::signal::ctrl_c;
use tokio::sync::Notify;
use tokio::time::{interval, sleep, Instant, Interval};
use tokio::{io, select};

use crate::cipher::CipherStream;
use crate::error::Error;
use crate::items::Stats;

use crate::options::{InstallOptions, Mode, Options, SetupMode};

mod cipher;
mod error;
#[cfg(feature = "installer")]
mod install;
mod items;
mod options;
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
// UDP header + ID + INDEX + DATA
const PACKET_SIZE: usize = 8 + ID_SIZE + INDEX_SIZE + TRANSFER_BUFFER_SIZE;

#[derive(Clone)]
struct TransferStats {
    confirmed_packets: Arc<AtomicUsize>,
    sent_packets: Arc<AtomicUsize>,
    total_data: Arc<AtomicUsize>,
    start_time: Instant,
    complete: Arc<AtomicBool>,
}

impl Default for TransferStats {
    fn default() -> Self {
        Self {
            confirmed_packets: Default::default(),
            sent_packets: Default::default(),
            total_data: Default::default(),
            start_time: Instant::now(),
            complete: Default::default(),
        }
    }
}

impl TransferStats {
    fn confirmed(&self) -> usize {
        self.confirmed_packets.load(Relaxed) * TRANSFER_BUFFER_SIZE
    }

    fn packet_loss(&self) -> f64 {
        let sent = self.sent_packets.load(Relaxed);

        if sent == 0 {
            return 0_f64;
        }

        let confirmed = self.confirmed_packets.load(Relaxed);
        let lost = sent.saturating_sub(confirmed) as f64;

        lost / sent as f64
    }

    fn total(&self) -> usize {
        self.total_data.load(Relaxed)
    }

    fn is_complete(&self) -> bool {
        self.complete.load(Relaxed)
    }

    fn speed(&self) -> f64 {
        self.confirmed() as f64 / self.start_time.elapsed().as_secs_f64() / 1_000_000_f64
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(feature = "installer")]
    {
        let args = env::args().collect::<Vec<_>>();

        // kinda sketchy but it works reliably
        if let Some(arg) = args.get(1) {
            if arg == "install" {
                // by skipping the first arg we can stop clap from tripping
                let options = InstallOptions::parse_from(&args[1..]);
                return install(options).await;
            }
        }
    }

    let mut options = Options::parse();
    let mut command = Options::command();

    let signal = Arc::new(Notify::new());

    // ctrl-c handler for attempting to gracefully exit
    tokio::spawn({
        let cancel_signal = signal.clone();

        async move {
            ctrl_c().await.expect("failed to listen for ctrl-c");
            debug!("ctrl-c received");
            cancel_signal.notify_waiters();
            sleep(Duration::from_secs(1)).await;
            ctrl_c().await.expect("failed to listen for ctrl-c");
            error!("ctrl-c received again. exiting");
            std::process::exit(1);
        }
    });

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

        let source_local = options.source.is_local();
        let destination_local = options.destination.is_local();

        if source_local && destination_local {
            command
                .error(
                    clap::error::ErrorKind::ValueValidation,
                    "both the source and destination cannot be local",
                )
                .exit();
        } else if !source_local && !destination_local {
            debug!("switching to controller mode");
            options.mode = Mode::Controller;
        }
    }

    let stats = TransferStats::default();

    match options.mode {
        Mode::Local => {
            let sender = options.source.is_local();

            let (local, remote) = if sender {
                (&options.source, &options.destination)
            } else {
                (&options.destination, &options.source)
            };

            debug!("local {}", local);
            debug!("remote {}", remote);

            // unwrap is safe because host must be specified for remote IoSpec
            let remote_addr = remote.host.unwrap();
            let remote_ip = remote_addr.ip();

            let client = connect_client(remote_addr, &remote.username).await?;

            info!("connected to the remote host via ssh");

            let command_handle = tokio::spawn({
                let stats = stats.clone();
                let command = options.format_command(sender, !options.stream_setup_mode);
                let signal = signal.clone();

                async move {
                    let status = command_runner(
                        client,
                        command,
                        sender.not().then_some(stats.sent_packets),
                        None,
                        None,
                        signal,
                    )
                    .await;

                    match status {
                        Ok(status) if status != 0 => Err(Error::command_failed(status)),
                        Err(error) => Err(error),
                        Ok(_) => {
                            debug!("remote command exited with status 0");
                            sleep(Duration::from_secs(u64::MAX)).await; // sleep forever to keep main future alive
                            Ok::<(), Error>(())
                        }
                    }
                }
            });

            let stats_handle = tokio::spawn({
                let stats = stats.clone();
                let interval = interval(Duration::from_millis(options.progress_interval));

                local_stats_printer(stats, interval)
            });

            let main_future = async {
                // receiver -> sender stream
                let rts = connect_stream(remote_ip, &mut options).await?;
                // sender -> receiver stream
                let str = connect_stream(remote_ip, &mut options).await?;

                run_transfer(sender, options, &stats, rts, str, remote_ip, signal).await
            };

            select! {
                result = command_handle => { debug!("command handle completed"); result?? },
                result = main_future => { debug!("main future completed"); result? }
            }

            stats.complete.store(true, Relaxed);
            stats_handle.await?;
        }
        Mode::Remote(sender) => {
            // remote clients must listen for STOP on stdin
            std::thread::spawn({
                let cancel_signal = signal.clone();

                move || {
                    wait_for_stop(cancel_signal);
                }
            });

            let stats_handle = tokio::spawn(remote_stats_printer(stats.clone()));

            match options.stream_setup_mode {
                // remote clients usually are in listen mode
                SetupMode::Listen => {
                    let (rts, addr) = listen_stream(&mut options).await?;
                    let (str, _) = listen_stream(&mut options).await?;

                    run_transfer(sender, options, &stats, rts, str, addr, signal).await?;
                }
                // remote clients only use connect mode for remote -> remote transfers where the source is always in connect mode
                SetupMode::Connect => {
                    // unwrap is safe because host must be specified for remote IoSpec
                    let addr = options.destination.host.unwrap().ip();

                    let rts = connect_stream(addr, &mut options).await?;
                    let str = connect_stream(addr, &mut options).await?;

                    run_transfer(sender, options, &stats, rts, str, addr, signal).await?;
                }
            }

            stats.complete.store(true, Relaxed);
            stats_handle.await?;
        }
        Mode::Controller => {
            // unwraps are safe because host must be specified for remote IoSpec
            let sender_addr = options.source.host.unwrap();
            let receiver_addr = options.destination.host.unwrap();

            let sender_handle = tokio::spawn(command_runner(
                connect_client(sender_addr, &options.source.username).await?,
                options.format_command(false, SetupMode::Connect), // sender is inverted somewhat confusingly
                // use the sender's stats
                Some(stats.sent_packets.clone()),
                Some(stats.confirmed_packets.clone()),
                Some(stats.total_data.clone()),
                signal.clone(),
            ));

            let receiver_handle = tokio::spawn(command_runner(
                connect_client(receiver_addr, &options.destination.username).await?,
                options.format_command(true, SetupMode::Listen),
                // ignore the receiver's stats
                None,
                None,
                None,
                signal.clone(),
            ));

            let stats_handle = tokio::spawn({
                let stats = stats.clone();
                let interval = interval(Duration::from_millis(options.progress_interval));

                local_stats_printer(stats, interval)
            });

            let status = select! {
                status = sender_handle => status??,
                status = receiver_handle => status??
            };

            if status != 0 {
                error!("remote returned status {}", status);
            }

            stats.complete.store(true, Relaxed);
            stats_handle.await?;
        }
    }

    info!("exiting");
    Ok(())
}

/// runs the installer
async fn install(options: InstallOptions) -> Result<()> {
    log_to_stderr(options.log_level);

    if let Some(host) = options.destination.host {
        let client = connect_client(host, &options.destination.username).await?;
        install::installer(client, options.destination.file_path, options.custom_binary).await
    } else {
        let mut command = InstallOptions::command();

        command
            .error(
                clap::error::ErrorKind::ValueValidation,
                "host must be specified",
            )
            .exit();
    }
}

/// selects the function to run based on the mode
#[inline]
async fn run_transfer(
    sender: bool,
    options: Options,
    stats: &TransferStats,
    rts: CipherStream,
    str: CipherStream,
    remote_addr: IpAddr,
    signal: Arc<Notify>,
) -> Result<()> {
    if sender {
        sender::main(options, stats, rts, str, remote_addr, signal).await
    } else {
        receiver::main(options, stats, rts, str, remote_addr, signal).await
    }
}

/// opens the sockets that will be used to send data
async fn socket_factory(
    start: u16,
    end: u16,
    remote_addr: IpAddr,
    threads: usize,
) -> io::Result<Vec<UdpSocket>> {
    iter(start..=end)
        .map(|port| async move {
            let socket = UdpSocket::bind(("0.0.0.0", port)).await?;
            socket.connect((remote_addr, port)).await?;
            Ok::<UdpSocket, io::Error>(socket)
        })
        .buffer_unordered(threads)
        .try_collect()
        .await
}

/// connects to a remote client via ssh
async fn connect_client(remote_addr: SocketAddr, username: &str) -> Result<Client> {
    let mut auth_method = get_auth(&remote_addr).await?;

    loop {
        match Client::connect(
            remote_addr,
            username,
            auth_method,
            ServerCheckMethod::NoCheck,
        )
        .await
        {
            Ok(client) => break Ok(client),
            Err(error) => match error {
                async_ssh2_tokio::error::Error::KeyAuthFailed => {
                    warn!("ssh key auth failed");
                    auth_method = password_auth(&remote_addr)?;
                }
                async_ssh2_tokio::error::Error::PasswordWrong => {
                    error!("invalid password");
                    auth_method = password_auth(&remote_addr)?;
                }
                _ => return Err(error.into()),
            },
        }
    }
}

/// select an auth method
async fn get_auth(host: &SocketAddr) -> io::Result<AuthMethod> {
    let mut auth = ssh_key_auth().await;

    if auth.is_err() {
        warn!("unable to load ssh key");
        auth = password_auth(host);
    }

    auth
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
fn password_auth(host: &SocketAddr) -> io::Result<AuthMethod> {
    let password = prompt_password(format!("{} password: ", host))?;
    Ok(AuthMethod::with_password(&password))
}

/// print a progress bar & stats to stdout
async fn local_stats_printer(stats: TransferStats, mut interval: Interval) {
    let bar = ProgressBar::new(100);

    bar.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}% [{msg}] ({eta})",
            )
            .unwrap()
            .progress_chars("=>-"),
    );

    while !stats.is_complete() {
        interval.tick().await;

        let progress = stats.confirmed() as f64 / stats.total() as f64 * 100_f64;
        let speed = stats.speed();
        let packet_loss = stats.packet_loss();

        bar.set_message(format!(
            "{:.1}MB/s {:.1}% packet loss",
            speed,
            packet_loss * 100_f64
        ));

        bar.set_position(progress as u64);
    }

    bar.finish_with_message("complete");
}

/// writes the Stats message into stdout
async fn remote_stats_printer(stats: TransferStats) {
    let mut interval = interval(Duration::from_secs(1));
    let mut stdout = stdout();

    while !stats.is_complete() {
        interval.tick().await;

        // convert the stats struct into a protobuf message
        let stats = Stats::from(&stats);

        // allocate a buffer for the message + newline
        let mut buf = Vec::with_capacity(stats.encoded_len() + 1);
        stats.encode(&mut buf).unwrap(); // infallible
        buf.push(b'\n'); // add a newline

        if let Err(error) = stdout.write(&buf).await {
            error!("failed to write stats to stdout: {}", error);
        }
    }
}

/// runs a command on the remote host & handles the output
async fn command_runner(
    client: Client,
    command: String,
    sent_packets: Option<Arc<AtomicUsize>>,
    confirmed_packets: Option<Arc<AtomicUsize>>,
    total_data: Option<Arc<AtomicUsize>>,
    cancel_signal: Arc<Notify>,
) -> Result<u32> {
    debug!("command runner starting for {:?}", command);

    let mut channel = client.get_channel().await?;
    channel.exec(true, command).await?;

    loop {
        select! {
            _ = cancel_signal.notified() => {
                // the remote client listens for STOP in it's stdin
                // this is more reliable & cross platform than sending a signal
                channel.data(&b"STOP\n"[..]).await?;
                debug!("sent STOP message");
            }
            message = channel.wait() => {
                if let Some(message) = message {
                    match message {
                        ChannelMsg::Data { ref data } => {
                            // the remote client sends stats messages to stdout
                            // the last byte is a newline
                            let stats = Stats::decode(&data[..data.len() - 1])?;

                            if let Some(sent_packets) = &sent_packets {
                                sent_packets.store(stats.sent_packets as usize, Relaxed);
                            }

                            if let Some(confirmed_packets) = &confirmed_packets {
                                confirmed_packets.store(stats.confirmed_packets as usize, Relaxed);
                            }

                            if let Some(total_data) = &total_data {
                                total_data.store(stats.total_data as usize, Relaxed);
                            }
                        }
                        ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                            let error = String::from_utf8_lossy(data).replace('\n', "");

                            if error.contains("not recognized as an internal or external command") {
                                break Err(Error::command_not_found());
                            } else {
                                error!("remote client stderr: {}", error);
                            }
                        }
                        ChannelMsg::ExitStatus { exit_status: 127 } => break Err(Error::command_not_found()),
                        ChannelMsg::ExitStatus { exit_status } => break Ok(exit_status),
                        _ => debug!("other message: {:?}", message),
                    }
                } else {
                    break Err(Error::no_exit_status())
                }
            }
        }
    }
}

/// connects to a listening remote client
async fn connect_stream(remote_addr: IpAddr, options: &mut Options) -> Result<CipherStream> {
    let tcp_stream = loop {
        if let Ok(stream) = TcpStream::connect((remote_addr, options.start_port)).await {
            break stream;
        } else {
            // give the listener time to start listening
            sleep(Duration::from_millis(100)).await;
        }
    };

    let stream = CipherStream::new(tcp_stream, &options.control_crypto)?;
    options.control_crypto.next_iv();
    options.start_port += 1;
    Ok(stream)
}

/// listens for a remote client to connect
async fn listen_stream(options: &mut Options) -> Result<(CipherStream, IpAddr)> {
    let listener = TcpListener::bind(("0.0.0.0", options.start_port)).await?;
    let (tcp_stream, remote_addr) = listener.accept().await?;

    let stream = CipherStream::new(tcp_stream, &options.control_crypto)?;
    options.control_crypto.next_iv();
    options.start_port += 1;
    Ok((stream, remote_addr.ip()))
}

/// calculate the BLAKE3 hash of a file
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

/// watches for stdin to receive a STOP message
fn wait_for_stop(signal: Arc<Notify>) {
    let stdin = stdin();
    let reader = std::io::BufReader::new(stdin);
    let lines = reader.lines();

    for line in lines.map_while(std::result::Result::ok) {
        if line.contains("STOP") {
            debug!("received STOP message");
            signal.notify_waiters();
            break;
        }
    }
}
