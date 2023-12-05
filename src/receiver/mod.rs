use std::mem;
use std::net::IpAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use deadqueue::limited::Queue;
use log::{debug, error, info, warn};
use tokio::fs::{metadata, rename};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout};
use tokio::{io, select};

use crate::receiver::writer::writer;
use crate::{
    socket_factory, LimitedQueue, Options, Result, TransferStats, UnlimitedQueue, INDEX_SIZE,
    MAX_RETRIES, RECEIVE_TIMEOUT, TRANSFER_BUFFER_SIZE,
};

mod writer;

type WriterQueue = LimitedQueue<Job>;

#[derive(Clone)]
struct Job {
    data: [u8; TRANSFER_BUFFER_SIZE], // the file chunk
    index: u64,                       // the index of the file chunk
}

pub(crate) async fn main(
    mut options: Options,
    stats: TransferStats,
    mut control_stream: TcpStream,
    remote_addr: IpAddr,
) -> Result<()> {
    // TODO what if the source is a directory?
    if options.destination.file_path.is_dir() {
        info!("destination is a folder, reformatting path with target file");

        options
            .destination
            .file_path
            .push(options.source.file_path.iter().last().unwrap())
    }

    info!("receiving {} -> {}", options.source, options.destination);

    let partial_path = options.destination.file_path.with_extension("partial");

    let start_index = if partial_path.exists() {
        info!("partial file exists, resuming transfer");
        let metadata = metadata(&partial_path).await?;
        // the file is written sequentially, so we can calculate the start index by rounding down to the nearest multiple of the transfer buffer size
        metadata.len().div_floor(TRANSFER_BUFFER_SIZE as u64) * TRANSFER_BUFFER_SIZE as u64
    } else {
        0
    };

    stats
        .confirmed_data
        .fetch_add(start_index as usize, Relaxed);

    // receive the file size from the remote client
    let file_size = control_stream.read_u64().await?;
    stats.total_data.store(file_size as usize, Relaxed);
    debug!("received file size: {}", file_size);

    // send the start index to the remote client
    debug!("sending start index {}", start_index);
    control_stream.write_u64(start_index).await?;

    let sockets = socket_factory(
        options.start_port + 1, // the first port is used for control messages
        options.end_port,
        remote_addr,
        options.threads,
    )
    .await?;

    info!("opened sockets");

    let writer_queue: WriterQueue = Arc::new(Queue::new(1_000));
    let confirmation_queue: UnlimitedQueue<u64> = Default::default();

    let writer_handle = tokio::spawn(writer(
        partial_path.clone(),
        writer_queue.clone(),
        file_size,
        confirmation_queue.clone(),
        start_index,
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
                &partial_path,
                &options.destination.file_path,
            )
            .await?;
        },
        _ = receiver_future => info!("receiver(s) exited"),
    }

    Ok(())
}

async fn receiver(queue: WriterQueue, socket: UdpSocket) {
    let mut buf = [0; INDEX_SIZE + TRANSFER_BUFFER_SIZE]; // buffer for receiving data
    let mut retries = 0; // counter to keep track of retries

    while retries < MAX_RETRIES {
        match timeout(RECEIVE_TIMEOUT, socket.recv(&mut buf)).await {
            Ok(Ok(read)) if read > 0 => {
                retries = 0; // reset retries
                let data = buf[INDEX_SIZE..].try_into().unwrap();
                let index = u64::from_be_bytes(buf[..INDEX_SIZE].try_into().unwrap());
                queue.push(Job { data, index }).await;
            }
            Ok(Ok(_)) => warn!("0 byte read?"), // this should never happen
            Ok(Err(_)) | Err(_) => retries += 1, // catch errors and timeouts
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

                // take the data out of the mutex
                let indexes = mem::take(&mut *data);
                drop(data); // release the lock on data

                write_indexes(&mut control_stream, &indexes).await?;
            }
        }
    });

    let future = async {
        loop {
            let index = queue.pop().await; // wait for a confirmation
            confirmed_data.fetch_add(TRANSFER_BUFFER_SIZE, Relaxed); // increment the confirmed data counter
            data.lock().await.push(index); // push the index to the data vector
        }
    };

    // propagate errors from the sender thread while executing the future
    select! {
        result = sender_handle => result?,
        _ = future => Ok(())
    }
}

// writes an array of u64 values to the control stream
async fn write_indexes<T: AsyncWrite + Unpin>(
    control_stream: &mut T,
    data: &[u64],
) -> io::Result<()> {
    let length = data.len() as u64;
    control_stream.write_u64(length).await?;

    // send the array of u64 values
    for value in data {
        control_stream.write_u64(*value).await?;
    }

    Ok(())
}
