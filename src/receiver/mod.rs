use std::mem;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use deadqueue::limited::Queue;
use log::{debug, error, info, warn};
use tokio::fs::rename;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout};
use tokio::{io, select};

use crate::receiver::metadata::Metadata;
use crate::receiver::writer::writer;
use crate::{socket_factory, LimitedQueue, Options, Result, TransferStats, UnlimitedQueue, INDEX_SIZE, MAX_RETRIES, TRANSFER_BUFFER_SIZE, RECEIVE_TIMEOUT};

mod metadata;
mod writer;

type WriterQueue = LimitedQueue<Job>;

#[derive(Clone)]
struct Job {
    data: [u8; TRANSFER_BUFFER_SIZE], // the file chunk
    index: u64,                       // the index of the file chunk
    len: usize,                       // the length of the file chunk
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

    let meta_data = Metadata::new(&options.destination.file_path).await?;
    stats
        .confirmed_data
        .fetch_add(meta_data.initial_index as usize, Relaxed);

    let listener = TcpListener::bind(("0.0.0.0", options.start_port)).await?;
    let (mut control_stream, _remote_addr) = listener.accept().await?;

    // receive the file size from the remote client
    let file_size = control_stream.read_u64().await?;
    stats.total_data.store(file_size as usize, Relaxed);
    debug!("received file size: {}", file_size);

    // send the start index to the remote client
    debug!("sending start index {}", meta_data.initial_index);
    control_stream.write_u64(meta_data.initial_index).await?;

    let sockets = socket_factory(
        options.start_port + 1, // the first port is used for control messages
        options.end_port,
        options.source.host.unwrap().as_str().parse()?,
        options.threads,
    )
    .await?;

    info!("opened sockets");

    let writer_queue: WriterQueue = Arc::new(Queue::new(1_000));
    let confirmation_queue: UnlimitedQueue<u64> = Default::default();

    let writer_handle = tokio::spawn(writer(
        options.destination.file_path.with_extension("partial"),
        writer_queue.clone(),
        file_size,
        confirmation_queue.clone(),
        meta_data,
    ));

    let confirmation_handle = tokio::spawn(send_confirmations(
        control_stream,
        confirmation_queue,
        stats.confirmed_data,
    ));

    let handles: Vec<_> = sockets
        .into_iter()
        .map(|socket| tokio::spawn(receiver(writer_queue.clone(), socket)))
        .collect();

    let receiver_future = async {
        for handle in handles {
            _ = handle.await;
        }
    };

    select! {
        result = confirmation_handle => error!("confirmation sender failed {:?}", result),
        result = writer_handle => {
            info!("writer finished with result {:?}", result);

            // rename the partial file to the original file
            rename(
                &options.destination.file_path.with_extension("partial"),
                &options.destination.file_path,
            )
            .await?;
        },
        _ = receiver_future => info!("receiver(s) exited"),
    }

    Ok(())
}

async fn receiver(queue: WriterQueue, socket: UdpSocket) {
    let mut buf = [0; INDEX_SIZE + TRANSFER_BUFFER_SIZE];
    let mut retries = 0;

    while retries < MAX_RETRIES {
        match timeout(RECEIVE_TIMEOUT, socket.recv(&mut buf)).await {
            Ok(Ok(read)) if read > 0 => {
                retries = 0;
                let data = buf[INDEX_SIZE..].try_into().unwrap();
                let index = u64::from_be_bytes(buf[..INDEX_SIZE].try_into().unwrap());
                let len = read - INDEX_SIZE;
                queue.push(Job { data, index, len }).await;
            }
            Ok(Ok(_)) => warn!("0 byte read?"),
            Ok(Err(error)) | Err(error) => {
                error!("failed to receive data {}", error);
                retries += 1;
            }
        }
    }
}

async fn send_confirmations(
    mut control_stream: TcpStream,
    queue: UnlimitedQueue<u64>,
    confirmed_data: Arc<AtomicUsize>,
) -> io::Result<()> {
    let data: Arc<Mutex<Vec<u64>>> = Default::default();

    let sender_handle: JoinHandle<io::Result<()>> = tokio::spawn({
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
                drop(data);

                send_indexes(&mut control_stream, &indexes).await?;
            }
        }
    });

    let future = async {
        loop {
            let index = queue.pop().await;
            confirmed_data.fetch_add(TRANSFER_BUFFER_SIZE, Relaxed);
            data.lock().await.push(index);
        }
    };

    select! {
        result = sender_handle => result?,
        _ = future => Ok(())
    }
}

// sends an array of indexes to the socket
async fn send_indexes(control_stream: &mut TcpStream, data: &[u64]) -> io::Result<()> {
    let length = data.len() as u64;
    control_stream.write_u64(length).await?;

    // send the array of u64 values
    for value in data {
        control_stream.write_u64(*value).await?;
    }

    Ok(())
}
