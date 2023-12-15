use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use aes::cipher::crypto_common::rand_core::OsRng;
use kanal::{AsyncReceiver, AsyncSender};
use log::{debug, error, info, warn};
use rand::RngCore;
use tokio::io::{self, AsyncReadExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::select;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::time::{interval, Instant};

use crate::error::Error;
use crate::items::{message, Confirmations, Crypto, FileDetail, Manifest, Message, StartIndex};
use crate::sender::reader::reader;
use crate::{
    hash_file, make_cipher, read_message, socket_factory, write_message, Options, Result,
    StreamCipherExt, TransferStats, ID_SIZE, INDEX_SIZE, MAX_RETRIES, REQUEUE_INTERVAL,
    TRANSFER_BUFFER_SIZE,
};

mod reader;

type JobCache = Arc<RwLock<HashMap<(u32, u64), Job>>>;

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

    let mut str_cipher = make_cipher(&options.control_crypto);
    let rts_cipher = make_cipher(&options.control_crypto);

    let mut manifest = build_manifest(
        options.source.file_path.clone(),
        options.verify,
        &stats.total_data,
        &options.stream_crypto,
    )
    .await?;

    debug!("sending manifest | files={} dirs={}", manifest.files.len(), manifest.directories.len());
    write_message(&mut str_stream, &manifest, &mut str_cipher).await?;

    let message: Message = read_message(&mut str_stream, &mut str_cipher).await?;

    match message.message {
        Some(message::Message::Completed(completed)) => {
            debug!("received {} completed ids", completed.ids.len());

            for id in completed.ids {
                if let Some(details) = manifest.files.remove(&id) {
                    stats.total_data.fetch_sub(details.size as usize, Relaxed);
                }
            }
        }
        Some(message::Message::Failure(failure)) => {
            error!("received failure message {}", failure.reason);
            return Err(Error::failure(failure.reason));
        }
        _ => unreachable!(),
    }

    let sockets = socket_factory(
        options.start_port + 2, // the first two ports are used for control messages and confirmations
        options.end_port,
        remote_addr,
        options.threads,
    )
    .await?;

    info!("opened sockets");

    // the reader fills the queue to 1_000 jobs, the unlimited capacity allows unconfirmed jobs to be added instantly
    let (job_sender, job_receiver) = kanal::unbounded_async();
    // a cache for the file chunks that have been sent but not confirmed
    let cache: JobCache = Default::default();

    // a semaphore to control the send rate
    let send = Arc::new(Semaphore::new(0));
    // a semaphore to control the readers
    let read = Arc::new(Semaphore::new(1_000));

    // just confirmation messages
    let (confirmation_sender, confirmation_receiver) = kanal::unbounded_async();
    // end and failure messages
    let (controller_sender, controller_receiver) = kanal::unbounded_async();

    // receive messages from the receiver into two channels based on message type
    let receiver_handle = tokio::spawn(split_receiver(
        rts_stream,
        confirmation_sender,
        controller_sender,
        rts_cipher,
    ));

    let confirmation_handle = tokio::spawn(receive_confirmations(
        confirmation_receiver,
        cache.clone(),
        job_sender.clone(),
        stats.confirmed_data.clone(),
        read.clone(),
    ));

    tokio::spawn(add_permits_at_rate(send.clone(), options.pps()));

    let controller_handle = tokio::spawn(controller(
        str_stream,
        manifest.files,
        job_sender.clone(),
        read,
        stats.confirmed_data,
        options.source.file_path,
        controller_receiver,
        options.max,
        str_cipher,
    ));

    let handles: Vec<_> = sockets
        .into_iter()
        .map(|socket| {
            tokio::spawn(sender(
                job_receiver.clone(),
                job_sender.clone(),
                socket,
                cache.clone(),
                send.clone(),
            ))
        })
        .collect();

    let sender_future = async {
        for handle in handles {
            handle.await??; // propagate errors
        }

        Ok(())
    };

    // propagate the first error
    select! {
        result = confirmation_handle => result?,
        result = controller_handle => result?,
        result = sender_future => result,
        result = receiver_handle => result?,
    }
}

async fn sender(
    job_receiver: AsyncReceiver<Job>,
    job_sender: AsyncSender<Job>,
    socket: UdpSocket,
    cache: JobCache,
    send: Arc<Semaphore>,
) -> Result<()> {
    let mut retries = 0;

    while retries < MAX_RETRIES {
        let permit = send.acquire().await?; // acquire a permit
        let mut job = job_receiver.recv().await?; // get a job from the queue

        // send the job data to the socket
        if let Err(error) = socket.send(&job.data).await {
            error!("failed to send data: {}", error);
            job_sender.send(job).await?; // put the job back in the queue
            retries += 1;
        } else {
            // cache the job
            job.cached_at = Some(Instant::now());
            cache.write().await.insert((job.id, job.index), job);
            retries = 0;
        }

        permit.forget();
    }

    if retries == MAX_RETRIES {
        Err(Error::max_retries())
    } else {
        Ok(())
    }
}

// TODO there is something wrong with the controller, seems to be starting the same file multiple times or something
#[allow(clippy::too_many_arguments)]
async fn controller<C: StreamCipherExt + ?Sized>(
    mut control_stream: TcpStream,
    mut files: HashMap<u32, FileDetail>,
    job_sender: AsyncSender<Job>,
    read: Arc<Semaphore>,
    confirmed_data: Arc<AtomicUsize>,
    base_path: PathBuf,
    controller_receiver: AsyncReceiver<Message>,
    max: usize,
    mut cipher: Box<C>,
) -> Result<()> {
    let mut id = 0;
    let mut active: HashMap<u32, FileDetail> = HashMap::with_capacity(max);

    loop {
        while active.len() < max && !files.is_empty() {
            match files.remove(&id) {
                None => id += 1,
                Some(details) => {
                    start_file_transfer(
                        &mut control_stream,
                        id,
                        &details,
                        &base_path,
                        &job_sender,
                        &read,
                        &confirmed_data,
                        &mut cipher,
                    )
                    .await?;

                    active.insert(id, details);
                    id += 1
                }
            }
        }

        debug!("waiting for a message");

        match controller_receiver.recv().await?.message {
            Some(message::Message::End(end)) => {
                if active.remove(&end.id).is_none() {
                    warn!("received end message for unknown file {}", end.id);
                } else {
                    debug!("received end message {} | active {}", end.id, active.len());
                }
            }
            Some(message::Message::Failure(failure)) => {
                if failure.reason == 0 {
                    if let Some(details) = active.get(&failure.id) {
                        warn!(
                            "transfer {} failed signature verification, retrying...",
                            failure.id
                        );

                        confirmed_data.fetch_sub(details.size as usize, Relaxed);

                        start_file_transfer(
                            &mut control_stream,
                            failure.id,
                            details,
                            &base_path,
                            &job_sender,
                            &read,
                            &confirmed_data,
                            &mut cipher,
                        )
                        .await?;
                    } else {
                        warn!(
                            "received failure message {} for unknown file {}",
                            failure.reason, failure.id
                        );
                    }
                } else {
                    warn!(
                        "received unknown failure message {} for {}",
                        failure.reason, failure.id
                    );
                }
            }
            _ => unreachable!(), // only end and failure messages are sent to this receiver
        }

        if files.is_empty() && active.is_empty() {
            break;
        }
    }

    debug!("all files completed, sending done message");
    write_message(&mut control_stream, &Message::done(), &mut cipher).await?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn start_file_transfer<C: StreamCipherExt + ?Sized>(
    mut control_stream: &mut TcpStream,
    id: u32,
    details: &FileDetail,
    base_path: &Path,
    job_sender: &AsyncSender<Job>,
    read: &Arc<Semaphore>,
    confirmed_data: &Arc<AtomicUsize>,
    cipher: &mut Box<C>,
) -> Result<()> {
    write_message(&mut control_stream, &Message::start(id), cipher).await?;

    let start_index: StartIndex = read_message(&mut control_stream, cipher).await?;
    confirmed_data.fetch_add(start_index.index as usize, Relaxed);

    tokio::spawn({
        let job_sender = job_sender.clone();
        let read = read.clone();
        let details = details.clone();
        let base_path = base_path.to_path_buf();

        async move {
            let result = reader(
                base_path.join(&details.path),
                job_sender,
                read,
                start_index.index,
                id,
                details.crypto.as_ref().map(make_cipher),
            )
            .await;

            if let Err(error) = result {
                error!("reader failed: {:?}", error);
            }
        }
    });

    Ok(())
}

async fn receive_confirmations(
    confirmation_receiver: AsyncReceiver<Confirmations>,
    cache: JobCache,
    job_sender: AsyncSender<Job>,
    confirmed_data: Arc<AtomicUsize>,
    read: Arc<Semaphore>,
) -> Result<()> {
    // this solves a problem where a confirmation is received after a job has already been requeued
    let lost_confirmations: Arc<Mutex<HashSet<(u32, u64)>>> = Default::default();

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
                for key in keys_to_remove {
                    if let Some(mut unconfirmed) = cache.remove(&key) {
                        if lost_confirmations.contains(&key) {
                            // the job is not requeued because it was confirmed while outside the cache
                            lost_confirmations.remove(&key);

                            read.add_permits(1);
                            confirmed_data.fetch_add(TRANSFER_BUFFER_SIZE, Relaxed);
                        } else {
                            unconfirmed.cached_at = None;
                            job_sender.send(unconfirmed).await.unwrap();
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
    debug!("adding permits at rate {}", rate);

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

/// split the message stream into `Confirmation` and `End + Failure` messages
async fn split_receiver<R: AsyncReadExt + Unpin, C: StreamCipherExt + ?Sized>(
    mut reader: R,
    confirmation_sender: AsyncSender<Confirmations>,
    controller_sender: AsyncSender<Message>,
    mut cipher: Box<C>,
) -> Result<()> {
    loop {
        let message: Message = read_message(&mut reader, &mut cipher).await?;

        match message.message {
            Some(message::Message::Confirmations(confirmations)) => {
                confirmation_sender.send(confirmations).await?
            }
            Some(message::Message::End(_)) => controller_sender.send(message).await?,
            Some(message::Message::Failure(_)) => controller_sender.send(message).await?,
            _ => {
                error!("received {:?}", message);
            }
        }
    }
}

async fn build_manifest(
    source: PathBuf,
    verify: bool,
    total_data: &Arc<AtomicUsize>,
    crypto: &Option<Crypto>,
) -> Result<Manifest> {
    // collect the files and directories to send
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    files_and_dirs(&source, &mut files, &mut dirs)?;

    let mut file_map: HashMap<u32, FileDetail> = HashMap::with_capacity(files.len());

    // TODO add concurrency
    for (index, mut file) in files.into_iter().enumerate() {
        let size = tokio::fs::metadata(&file).await?.len();
        total_data.fetch_add(size as usize, Relaxed);

        let signature = if verify {
            let hash = hash_file(&file).await?;
            Some(hash.as_bytes().to_vec())
        } else {
            None
        };

        if file == source {
            file = PathBuf::from(file.iter().last().unwrap())
        } else {
            file = file.strip_prefix(&source).unwrap().to_path_buf();
        }

        // TODO windows only
        let path = file.to_string_lossy().replace('\\', "/");

        let mut crypto = crypto.clone();

        if let Some(ref mut crypto) = crypto {
            OsRng.fill_bytes(&mut crypto.iv);
        }

        file_map.insert(
            index as u32,
            FileDetail {
                path,
                size,
                signature,
                crypto,
            },
        );
    }

    let directories = dirs
        .into_iter()
        .map(|dir| {
            if let Ok(file_path) = dir.strip_prefix(&source) {
                file_path.to_string_lossy().to_string()
            } else {
                dir.to_string_lossy().to_string()
            }
        })
        .map(|dir| dir.replace('\\', "/")) // TODO windows only
        .collect();

    let manifest = Manifest {
        directories,
        files: file_map,
    };

    Ok(manifest)
}
