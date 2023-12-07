use async_channel::{Receiver, Sender};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info, warn};
use tokio::io::{self, AsyncReadExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::select;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::time::{interval, Instant};

use crate::items::{
    message, Confirmations, Done, End, FileDetail, Files, Message, Start, StartIndex,
};
use crate::sender::reader::reader;
use crate::{
    read_message, socket_factory, write_message, Options, Result, TransferStats, UnlimitedQueue,
    ID_SIZE, INDEX_SIZE, MAX_CONCURRENT_TRANSFERS, MAX_RETRIES, REQUEUE_INTERVAL,
    TRANSFER_BUFFER_SIZE,
};

mod reader;

type JobQueue = UnlimitedQueue<Job>;
type JobCache = Arc<RwLock<BTreeMap<(u32, u64), Job>>>;

struct Job {
    data: [u8; ID_SIZE + INDEX_SIZE + TRANSFER_BUFFER_SIZE],
    index: u64,
    id: u32,
    cached_at: Option<Instant>,
}

pub(crate) async fn main(
    options: Options,
    stats: TransferStats,
    rts_stream: TcpStream,
    mut str_stream: TcpStream,
    remote_addr: IpAddr,
) -> Result<()> {
    info!("sending {} -> {}", options.source, options.destination);

    // collect the files and directories to send
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    files_and_dirs(&options.source.file_path, &mut files, &mut dirs)?;

    let mut file_map: HashMap<u32, FileDetail> = HashMap::with_capacity(files.len());

    for (index, file) in files.into_iter().enumerate() {
        let file_size = tokio::fs::metadata(&file).await?.len();
        stats.total_data.fetch_add(file_size as usize, Relaxed);

        if let Ok(file_path) = file.strip_prefix(&options.source.file_path) {
            file_map.insert(
                index as u32,
                FileDetail {
                    file_path: file_path.to_string_lossy().to_string(),
                    file_size,
                },
            );
        }
    }

    let directories = dirs
        .into_iter()
        .map(|dir| {
            if let Ok(file_path) = dir.strip_prefix(&options.source.file_path) {
                file_path.to_string_lossy().to_string()
            } else {
                dir.to_string_lossy().to_string()
            }
        })
        .collect();

    let files = Files {
        directories,
        files: file_map,
    };

    debug!("sending files: {:?}", files);
    write_message(&mut str_stream, &files).await?;

    let sockets = socket_factory(
        options.start_port + 2, // the first two ports are used for control messages and confirmations
        options.end_port,
        remote_addr,
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
    // a map of semaphores to control the reads for each file
    let mut read_semaphores: HashMap<u32, Arc<Semaphore>> = Default::default();

    // create a semaphore for each file
    for id in files.files.keys() {
        let read = Arc::new(Semaphore::new(1_000));
        read_semaphores.insert(*id, read);
    }

    let read_semaphores = Arc::new(read_semaphores);

    let (confirmation_sender, confirmation_receiver) = async_channel::unbounded();
    let (end_sender, end_receiver) = async_channel::unbounded();

    tokio::spawn(split_receiver(rts_stream, confirmation_sender, end_sender));

    let confirmation_handle = tokio::spawn({
        let cache = cache.clone();
        let queue = queue.clone();
        let read_semaphores = read_semaphores.clone();

        receive_confirmations(
            confirmation_receiver,
            cache,
            queue,
            stats.confirmed_data.clone(),
            read_semaphores,
        )
    });

    let semaphore = send.clone();
    tokio::spawn(add_permits_at_rate(semaphore, options.rate));

    let controller_handle = tokio::spawn(controller(
        str_stream,
        files,
        queue.clone(),
        read_semaphores,
        stats.confirmed_data,
        options.source.file_path,
        end_receiver,
    ));

    let handles: Vec<_> = sockets
        .into_iter()
        .map(|socket| tokio::spawn(sender(queue.clone(), socket, cache.clone(), send.clone())))
        .collect();

    let sender_future = async {
        for handle in handles {
            _ = handle.await;
        }
    };

    select! {
        result = confirmation_handle => result?,
        result = controller_handle => result?,
        _ = sender_future => { warn!("senders exited"); Ok(()) },
    }
}

async fn sender(queue: JobQueue, socket: UdpSocket, cache: JobCache, send: Arc<Semaphore>) {
    let mut retries = 0;

    while retries < MAX_RETRIES {
        let permit = send.acquire().await.unwrap(); // acquire a permit
        let mut job = queue.pop().await; // get the next job

        // send the job data to the socket
        if let Err(error) = socket.send(&job.data).await {
            error!("failed to send data: {}", error);
            queue.push(job); // put the job back in the queue
            retries += 1;
        } else {
            // cache the job
            job.cached_at = Some(Instant::now());
            cache.write().await.insert((job.id, job.index), job);
            retries = 0;
        }

        permit.forget();
    }
}

async fn controller(
    mut control_stream: TcpStream,
    mut files: Files,
    job_queue: JobQueue,
    read_semaphores: Arc<HashMap<u32, Arc<Semaphore>>>,
    confirmed_data: Arc<AtomicUsize>,
    file_path: PathBuf,
    end_receiver: Receiver<End>,
) -> Result<()> {
    let mut id = 0;
    let mut active = 0;

    loop {
        while active < MAX_CONCURRENT_TRANSFERS {
            match files.files.remove(&id) {
                None => break,
                Some(file_details) => {
                    let read = read_semaphores.get(&id).unwrap().clone();

                    let message = Message {
                        message: Some(message::Message::Start(Start { id })),
                    };
                    write_message(&mut control_stream, &message).await?;

                    let file_path = file_path.join(&file_details.file_path);

                    let start_index: StartIndex = read_message(&mut control_stream).await?;
                    confirmed_data.store(start_index.index as usize, Relaxed);

                    tokio::spawn(reader(
                        file_path,
                        job_queue.clone(),
                        read,
                        start_index.index,
                        id,
                    ));

                    id += 1;
                    active += 1;
                }
            }
        }

        debug!("started max files, waiting for end message");
        let end = end_receiver.recv().await.unwrap();
        debug!("received end message: {:?} | active {}", end, active);
        active -= 1;

        if files.files.is_empty() && active == 0 {
            break;
        }
    }

    debug!("all files completed, sending done message");

    let message = Message {
        message: Some(message::Message::Done(Done {})),
    };
    write_message(&mut control_stream, &message).await?;

    Ok(())
}

async fn receive_confirmations(
    confirmation_receiver: Receiver<Confirmations>,
    cache: JobCache,
    queue: JobQueue,
    confirmed_data: Arc<AtomicUsize>,
    read_semaphores: Arc<HashMap<u32, Arc<Semaphore>>>,
) -> Result<()> {
    // this solves a problem where a confirmation is received after a job has already been requeued
    let lost_confirmations: Arc<Mutex<HashSet<(u32, u64)>>> = Default::default();

    // this thread checks the cache for unconfirmed jobs that have been there for too long and requeues them
    tokio::spawn({
        let cache = cache.clone();
        let lost_confirmations = lost_confirmations.clone();
        let confirmed_data = confirmed_data.clone();
        let read_semaphores = read_semaphores.clone();

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
                            lost_confirmations.remove(&key);

                            let read = read_semaphores.get(&key.0).unwrap();
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

    while let Ok(confirmations) = confirmation_receiver.recv().await {
        let mut lost_confirmations = lost_confirmations.lock().await;
        let mut cache = cache.write().await;

        for (id, indexes) in confirmations.indexes {
            let read = read_semaphores.get(&id).unwrap();

            // process the array of indexes
            for index in indexes.inner {
                if cache.remove(&(id, index)).is_none() {
                    // if the index is not in the cache, it was already requeued
                    lost_confirmations.insert((id, index));
                } else {
                    read.add_permits(1); // add a permit to the reader
                    confirmed_data.fetch_add(TRANSFER_BUFFER_SIZE, Relaxed);
                }
            }
        }
    }

    Ok(())
}

/// adds leases to the semaphore at a given rate
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

/// recursively collect all files and directories in a directory
fn files_and_dirs(
    source: &Path,
    files: &mut Vec<PathBuf>,
    dirs: &mut Vec<PathBuf>,
) -> io::Result<()> {
    if source.is_dir() {
        for entry in source.read_dir()?.filter_map(std::result::Result::ok) {
            let path = entry.path();

            if path.is_dir() {
                dirs.push(path.clone());
                files_and_dirs(&path, files, dirs)?;
            } else {
                files.push(path);
            }
        }
    } else {
        files.push(source.to_path_buf());
    }

    Ok(())
}

/// split the message stream into `Confirmation` and `End` messages
async fn split_receiver<R: AsyncReadExt + Unpin>(
    mut reader: R,
    confirmation_sender: Sender<Confirmations>,
    end_sender: Sender<End>,
) -> Result<()> {
    loop {
        let message: Message = read_message(&mut reader).await?;

        match message.message {
            Some(message::Message::Confirmations(confirmations)) => {
                confirmation_sender
                    .send(confirmations)
                    .await
                    .expect("failed to send confirmations");
            }
            Some(message::Message::End(end)) => {
                end_sender.send(end).await.expect("failed to send end");
            }
            _ => {
                error!("received {:?}", message);
            }
        }
    }
}
