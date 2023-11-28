use std::mem;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info, warn};
use tokio::fs::rename;
use tokio::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Mutex;
use tokio::time::{interval, sleep};

use crate::receiver::metadata::Metadata;
use crate::receiver::writer::writer;
use crate::{
    socket_factory, Options, Queue, Result, TransferStats, INDEX_SIZE, MAX_RETRIES,
    TRANSFER_BUFFER_SIZE,
};

mod metadata;
mod writer;

pub(crate) async fn main(mut options: Options, stats: TransferStats) -> Result<()> {
    if options.destination.file_path.is_dir() {
        info!("destination is a folder, reformatting path with target file");

        options
            .destination
            .file_path
            .push(options.source.file_path.iter().last().unwrap())
    }

    info!("receiving {} -> {}", options.source, options.destination);

    let (file_size, mut socket) = receive_manifest(options.start_port).await?;
    stats.total_data.store(file_size as usize, Relaxed);
    debug!("received manifest: {}", file_size);

    let meta_data = Metadata::new(&options.destination.file_path).await?;
    stats
        .confirmed_data
        .fetch_add(meta_data.initial_index as usize, Relaxed);

    debug!("sending start index {}", meta_data.initial_index);
    socket
        .write_all(&meta_data.initial_index.to_be_bytes())
        .await?;

    let sockets = socket_factory(
        options.start_port + 1, // the first port is used for control messages
        options.end_port,
        options.source.host.unwrap().as_str().parse().unwrap(),
        options.threads,
    )
    .await?;

    info!("opened sockets");

    let writer_queue: Queue<Vec<u8>> = Default::default();
    let confirmation_queue: Queue<u64> = Default::default();

    let writer_handle = tokio::spawn(writer(
        options.destination.file_path.with_extension("partial"),
        writer_queue.clone(),
        file_size,
        confirmation_queue.clone(),
        meta_data,
    ));

    tokio::spawn(async {
        if let Err(error) =
            send_confirmations(socket, confirmation_queue, stats.confirmed_data).await
        {
            error!("confirmation sender failed: {}", error);
        }
    });

    let _: Vec<_> = sockets
        .into_iter()
        .map(|socket| tokio::spawn(receiver(writer_queue.clone(), socket)))
        .collect();

    let writer_result = writer_handle.await;
    info!("writer finished with result {:?}", writer_result);

    // rename the partial file to the original file
    rename(
        &options.destination.file_path.with_extension("partial"),
        &options.destination.file_path,
    )
    .await?;

    Ok(())
}

pub(crate) async fn receiver(queue: Queue<Vec<u8>>, socket: UdpSocket) {
    let mut buf = [0; INDEX_SIZE + TRANSFER_BUFFER_SIZE];
    let mut retries = 0;

    loop {
        match socket.recv(&mut buf).await {
            Ok(read) => {
                retries = 0;

                if read > 0 {
                    queue.push(buf[..read].to_vec());
                } else {
                    warn!("0 byte read?");
                }
            }
            Err(error) => {
                error!("failed to receive data {}", error);

                if retries < MAX_RETRIES {
                    retries += 1;
                } else {
                    break;
                }
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

async fn send_confirmations(
    mut socket: TcpStream,
    queue: Queue<u64>,
    confirmed_data: Arc<AtomicUsize>,
) -> io::Result<()> {
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

                let indexes = mem::take(&mut *data);

                if let Err(error) = send_indexes(&mut socket, &indexes).await {
                    error!("failed to send indexes: {}", error);
                    sleep(Duration::from_millis(10)).await;
                }
            }
        }
    });

    loop {
        let index = queue.pop().await;
        confirmed_data.fetch_add(TRANSFER_BUFFER_SIZE, Relaxed);
        data.lock().await.push(index);
    }
}

// sends an array of indexes to the socket
async fn send_indexes(socket: &mut TcpStream, data: &[u64]) -> io::Result<()> {
    let length = data.len() as u64;
    socket.write_all(&length.to_be_bytes()).await?;

    // send the array of u64 values
    for value in data {
        socket.write_all(&value.to_be_bytes()).await?;
    }

    Ok(())
}
