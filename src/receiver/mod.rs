use std::mem;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use async_channel::Receiver;
use deadqueue::limited::Queue;
use log::{debug, error, info, warn};
use tokio::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Mutex;
use tokio::time::{interval, Instant};

use crate::receiver::writer::writer;
use crate::{socket_factory, ByteQueue, Options, INDEX_SIZE, TRANSFER_BUFFER_SIZE, TransferStats};

mod writer;

type Result<T> = std::result::Result<T, ReceiveError>;

#[derive(Debug)]
pub(crate) struct ReceiveError {
    _kind: ReceiveErrorKind
}

#[derive(Debug)]
enum ReceiveErrorKind {
    IoError(io::Error)
}

impl From<io::Error> for ReceiveError {
    fn from(error: io::Error) -> Self {
        Self {
            _kind: ReceiveErrorKind::IoError(error)
        }
    }
}

pub(crate) async fn main(mut options: Options, stats: TransferStats) -> Result<()> {
    if options.destination.file_path.is_dir() {
        info!("destination is a folder, reformatting path with target file");

        options
            .destination
            .file_path
            .push(options.source.file_path.iter().last().unwrap())
    }

    info!("receiving {} -> {}", options.source, options.destination);

    let (file_size, socket) = receive_manifest(options.start_port).await?;
    stats.total_data.store(file_size as usize, Relaxed);
    debug!("received manifest: {}", file_size);

    let sockets = socket_factory(
        options.start_port + 1, // the first port is used for control messages
        options.end_port,
        options.source.host.unwrap().as_str().parse().unwrap(),
        options.threads,
    )
    .await?;

    info!("opened sockets");

    let queue = Arc::new(Queue::new(10_000));
    let (confirmation_sender, confirmation_receiver) = async_channel::unbounded();

    let writer_handle = tokio::spawn(writer(
        options.destination.file_path,
        queue.clone(),
        file_size,
        confirmation_sender,
    ));

    let confirmation_handle = tokio::spawn(async {
        if let Err(error) = send_confirmations(socket, confirmation_receiver, stats.confirmed_data).await {
            error!("confirmation sender failed: {}", error);
        }
    });

    let _: Vec<_> = sockets
        .into_iter()
        .map(|socket| tokio::spawn(receiver(queue.clone(), socket)))
        .collect();

    let now = Instant::now();

    let writer_result = writer_handle.await;
    info!("writer finished with result {:?}", writer_result);

    debug!("waiting for confirmation sender to finish");
    if let Err(error) = confirmation_handle.await {
        error!("confirmation sender exited with error: {}", error);
    }

    let elapsed = now.elapsed().as_secs();
    let speed = file_size as f64 / elapsed as f64 / 1_000_000_f64;
    info!(
        "received {} bytes in {} seconds ({} MB/s)",
        file_size, elapsed, speed
    );

    Ok(())
}

pub(crate) async fn receiver(queue: ByteQueue, socket: UdpSocket) {
    let mut buf = [0; 4 + INDEX_SIZE + TRANSFER_BUFFER_SIZE];

    loop {
        match socket.recv(&mut buf).await {
            Ok(read) => {
                if read > 0 {
                    queue.push(buf[..read].to_vec()).await;
                } else {
                    warn!("0 byte read?");
                }
            }
            Err(error) => {
                error!("receiver error {}", error);
            }
        }
    }
}

async fn receive_manifest(port: u16) -> io::Result<(u64, TcpStream)> {
    info!("listening for manifest on 0.0.0.0:{}", port);

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;

    let (mut stream, _remote_addr) = listener.accept().await?;

    let mut buf = [0; 8];
    stream.read_exact(&mut buf).await?;

    Ok((u64::from_be_bytes(buf), stream))
}

async fn send_confirmations(mut socket: TcpStream, queue: Receiver<u64>, confirmed_data: Arc<AtomicUsize>) -> io::Result<()> {
    let data: Arc<Mutex<Vec<u64>>> = Default::default();

    tokio::spawn({
        let data = data.clone();

        async move {
            let mut interval = interval(Duration::from_millis(10));

            loop {
                interval.tick().await;

                let mut data = data.lock().await;

                if data.is_empty() {
                    continue;
                }

                let taken_vec = mem::take(&mut *data);
                send(&mut socket, taken_vec).await.unwrap();
            }
        }
    });

    while let Ok(index) = queue.recv().await {
        confirmed_data.fetch_add(TRANSFER_BUFFER_SIZE, Relaxed);
        data.lock().await.push(index);
    }

    Ok(())
}

// sends an array of indexes to the socket
async fn send(socket: &mut TcpStream, data: Vec<u64>) -> io::Result<()> {
    let length = data.len() as u64;
    socket.write_all(&length.to_be_bytes()).await?;

    // send the array of u64 values
    for value in &data {
        socket.write_all(&value.to_be_bytes()).await?;
    }

    Ok(())
}
