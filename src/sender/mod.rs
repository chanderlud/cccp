use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info};
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::select;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::time::{interval, sleep, Instant};

use crate::{
    socket_factory, Options, Result, TransferStats, UnlimitedQueue, MAX_RETRIES, REQUEUE_INTERVAL,
    TRANSFER_BUFFER_SIZE,
};

mod reader;

type JobQueue = UnlimitedQueue<Job>;
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

    // connect to the remote client on the first port in the range
    let mut control_stream = TcpStream::connect((remote_address, options.start_port)).await?;
    // send the file size to the remote client
    control_stream.write_u64(file_size).await?;

    // receive the start index from the remote client
    let start_index = control_stream.read_u64().await?;
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

    tokio::spawn(reader::reader(
        options.source.file_path,
        queue.clone(),
        read.clone(),
        start_index,
    ));

    let confirmation_handle = tokio::spawn({
        let cache = cache.clone();
        let queue = queue.clone();
        let read = read.clone();

        receive_confirmations(control_stream, cache, queue, stats.confirmed_data, read)
    });

    let semaphore = send.clone();
    tokio::spawn(add_permits_at_rate(semaphore, options.rate));

    let handles: Vec<_> = sockets
        .into_iter()
        .map(|socket| tokio::spawn(sender(queue.clone(), socket, cache.clone(), send.clone())))
        .collect();

    let sender_future = async {
        for handle in handles {
            _ = handle.await;
        }
    };

    // let reader_future = async {
    //     _ = reader_handle.await;
    //     info!("reader exited");
    //
    //     while !queue.is_empty() && !cache.read().await.is_empty() {
    //         sleep(Duration::from_secs(1)).await;
    //     }
    //
    //     info!("the queue and cache emptied, so hopefully all the data was sent");
    // };

    select! {
        // _ = reader_future => {},
        _ = sender_future => error!("senders exited"),
        result = confirmation_handle => {
            // the confirmation receiver never exits unless an error occurs
            error!("confirmation receiver exited with result {:?}", result);
        }
    }

    Ok(())
}

async fn sender(queue: JobQueue, socket: UdpSocket, cache: JobCache, send: Arc<Semaphore>) {
    let mut retries = 0;

    loop {
        let permit = send.acquire().await.unwrap(); // acquire a permit
        let mut job = queue.pop().await; // get the next job

        // send the job data to the socket
        if let Err(error) = socket.send(&job.data).await {
            error!("failed to send data: {}", error);
            queue.push(job); // put the job back in the queue

            if retries < MAX_RETRIES {
                retries += 1;
            } else {
                error!("sender: too many retries, exiting");
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

async fn receive_confirmations(
    mut control_stream: TcpStream,
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
        let confirmed_data = confirmed_data.clone();
        let read = read.clone();

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
                for index in keys_to_remove {
                    if let Some(mut unconfirmed) = cache.remove(&index) {
                        if lost_confirmations.contains(&index) {
                            // the job is not requeued because it was confirmed while outside the cache
                            lost_confirmations.remove(&index);

                            read.add_permits(1);
                            confirmed_data.fetch_add(TRANSFER_BUFFER_SIZE, Relaxed);
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
        let confirmed_indexes = receive_indexes(&mut control_stream).await?;

        let mut lost_confirmations = lost_confirmations.lock().await;
        let mut cache = cache.write().await;

        // process the array of u64 values
        for index in confirmed_indexes {
            if cache.remove(&index).is_none() {
                // if the index is not in the cache, it was already requeued
                lost_confirmations.insert(index);
            } else {
                read.add_permits(1); // add a permit to the reader
                confirmed_data.fetch_add(TRANSFER_BUFFER_SIZE, Relaxed);
            }
        }
    }
}

// adds leases to the semaphore at a given rate to control the send rate
async fn add_permits_at_rate(semaphore: Arc<Semaphore>, rate: u64) {
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

async fn receive_indexes(control_stream: &mut TcpStream) -> io::Result<Vec<u64>> {
    let length = control_stream.read_u64().await? as usize; // read the length of the array
    let mut indexes = Vec::with_capacity(length); // create a vector with the capacity of the array

    for _ in 0..length {
        let index = control_stream.read_u64().await?; // read the u64 value
        indexes.push(index);
    }

    Ok(indexes)
}
