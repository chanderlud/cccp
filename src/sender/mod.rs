use std::collections::{BTreeMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info, warn};
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::select;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::time::{interval, sleep, Instant};

use crate::{
    socket_factory, Options, Queue, Result, TransferStats, MAX_RETRIES, REQUEUE_INTERVAL,
    TRANSFER_BUFFER_SIZE,
};

mod reader;

type JobQueue = Queue<Job>;
type JobCache = Arc<RwLock<BTreeMap<u64, Job>>>;

#[derive(Clone)]
pub(crate) struct Job {
    data: Vec<u8>, // index (8 bytes) + the file chunk
    index: u64,    // the index of the file chunk
    cached_at: Option<Instant>,
}

pub(crate) async fn main(options: Options, stats: TransferStats) -> Result<()> {
    info!("sending {:?} to {:?}", options.source, options.destination);

    let remote_address = options.destination.host.unwrap().parse()?;
    let file_size = options.source.file_size().await?;
    stats.total_data.store(file_size as usize, Relaxed);

    // give the receiver time to start listening
    sleep(Duration::from_millis(1_000)).await;

    let mut socket = send_manifest(remote_address, options.start_port, file_size).await?;

    let mut buf = [0; 8];
    socket.read_exact(&mut buf).await?;
    let start_index = u64::from_be_bytes(buf);
    stats.confirmed_data.store(start_index as usize, Relaxed);

    debug!("received start index {}", start_index);

    let sockets = socket_factory(
        options.start_port + 1, // the first port is used for control messages
        options.end_port,
        remote_address,
        options.threads,
    )
    .await?;

    info!("opened sockets");

    // the reader fills the queue to 1_000 jobs, the unlimited capacity allows unconfirmed jobs to be added instantly
    let queue: JobQueue = Default::default();
    // a cache for the file chunks that have been sent but not confirmed
    let cache: JobCache = Default::default();

    // a semaphore to control the send rate
    let send = Arc::new(Semaphore::new(0));
    // a semaphore which limits the number of jobs that the reader will add to the queue
    let read = Arc::new(Semaphore::new(1_000));

    let reader_handle = tokio::spawn(reader::reader(
        options.source.file_path,
        queue.clone(),
        read.clone(),
        start_index,
    ));

    let confirmation_handle = tokio::spawn({
        let cache = cache.clone();
        let queue = queue.clone();
        let read = read.clone();

        receive_confirmations(socket, cache, queue, stats.confirmed_data, read)
    });

    let semaphore = send.clone();
    tokio::spawn(add_leases_at_rate(semaphore, options.rate));

    let handles: Vec<_> = sockets
        .into_iter()
        .map(|socket| tokio::spawn(sender(queue.clone(), socket, cache.clone(), send.clone())))
        .collect();

    let sender_future = async {
        for handle in handles {
            let _ = handle.await;
        }
    };

    let reader_future = async {
        _ = reader_handle.await;
        info!("reader exited");

        while !queue.is_empty() && !cache.read().await.is_empty() {
            sleep(Duration::from_secs(1)).await;
        }

        info!("the queue and cache emptied, so hopefully all the data was sent");
    };

    select! {
        _ = reader_future => {},
        _ = sender_future => { warn!("senders exited") },
        result = confirmation_handle => {
            info!("confirmation sender exited with result {:?}", result);
        }
    }

    Ok(())
}

async fn sender(queue: JobQueue, socket: UdpSocket, cache: JobCache, send: Arc<Semaphore>) {
    let mut retries = 0;

    loop {
        let mut job = queue.pop().await; // get the next job
        let permit = send.acquire().await.unwrap(); // acquire a permit

        // send the job data to the socket
        if let Err(error) = socket.send(&job.data).await {
            error!("failed to send data: {}", error);
            queue.push(job); // put the job back in the queue

            if retries < MAX_RETRIES {
                retries += 1;
            } else {
                break;
            }
        } else {
            // cache the job
            job.cached_at = Some(Instant::now());
            cache.write().await.insert(job.index, job);
            retries = 0;
        }

        permit.forget();
    }
}

async fn send_manifest(remote_address: IpAddr, port: u16, length: u64) -> io::Result<TcpStream> {
    info!("sending manifest to {}:{}", remote_address, port);

    let mut socket = TcpStream::connect((remote_address, port)).await?;

    socket.write_all(&length.to_be_bytes()).await?;

    Ok(socket)
}

async fn receive_confirmations(
    mut socket: TcpStream,
    cache: JobCache,
    queue: JobQueue,
    confirmed_data: Arc<AtomicUsize>,
    read: Arc<Semaphore>,
) -> io::Result<()> {
    // this solves a problem where a confirmation is received after a job has already been requeued
    let lost_confirmations: Arc<Mutex<HashSet<u64>>> = Default::default();

    // this thread checks the cache for unconfirmed jobs that have been there for too long and requeues them
    tokio::spawn({
        let cache = cache.clone();
        let lost_confirmations = lost_confirmations.clone();
        let mut interval = interval(Duration::from_millis(100));

        async move {
            loop {
                interval.tick().await;

                // collect keys of the entries to remove
                let keys_to_remove: Vec<_> = cache
                    .read()
                    .await
                    .iter()
                    .filter(|(_, unconfirmed)| {
                        unconfirmed.cached_at.unwrap().elapsed() > REQUEUE_INTERVAL
                    })
                    .map(|(key, _)| *key)
                    .collect();

                let mut lost_confirmations = lost_confirmations.lock().await;
                let mut cache = cache.write().await;

                // requeue and remove entries
                for key in keys_to_remove {
                    if let Some(mut unconfirmed) = cache.remove(&key) {
                        if lost_confirmations.contains(&key) {
                            // the job is not requeued because it was confirmed while outside the cache
                            debug!("found lost confirmation for {}", key);
                            lost_confirmations.remove(&key);
                        } else {
                            unconfirmed.cached_at = None;
                            queue.push(unconfirmed);
                        }
                    }
                }
            }
        }
    });

    loop {
        socket.flush().await?;

        let confirmed_indexes = receive_indexes(&mut socket).await?;
        let length = confirmed_indexes.len();

        confirmed_data.fetch_add(length * TRANSFER_BUFFER_SIZE, Relaxed);
        read.add_permits(length); // add a permit to the reader for each confirmed index

        let mut lost_confirmations = lost_confirmations.lock().await;
        let mut cache = cache.write().await;

        // process the array of u64 values
        for index in confirmed_indexes {
            if cache.remove(&index).is_none() {
                // if the index is not in the cache, it was already requeued
                lost_confirmations.insert(index);
            }
        }
    }
}

// adds leases to the semaphore at a given rate to control the send rate
async fn add_leases_at_rate(semaphore: Arc<Semaphore>, rate: u64) {
    let mut interval = interval(Duration::from_nanos(1_000_000_000 / rate));

    loop {
        interval.tick().await;

        // don't want too many permits to build up
        if semaphore.available_permits() < 1_000 {
            // add a lease to the semaphore
            semaphore.add_permits(1);
        }
    }
}

async fn receive_indexes(socket: &mut TcpStream) -> io::Result<Vec<u64>> {
    let mut buffer = Vec::new();
    let mut length_buffer = [0; 8];

    socket.read_exact(&mut length_buffer).await?;
    let length = u64::from_be_bytes(length_buffer) as usize;

    if length == 0 {
        debug!("received empty array of indexes");
        return Ok(Vec::new());
    }

    // resize the buffer to hold all u64 values
    buffer.resize(length * 8, 0);

    // read the array of u64 values
    socket.read_exact(&mut buffer).await?;

    Ok((0..length)
        .map(|i| {
            u64::from_be_bytes([
                buffer[i * 8],
                buffer[i * 8 + 1],
                buffer[i * 8 + 2],
                buffer[i * 8 + 3],
                buffer[i * 8 + 4],
                buffer[i * 8 + 5],
                buffer[i * 8 + 6],
                buffer[i * 8 + 7],
            ])
        })
        .collect())
}
