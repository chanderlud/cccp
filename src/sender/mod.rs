use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::iter;
use futures::{StreamExt, TryStreamExt};
use kanal::{AsyncReceiver, AsyncSender};
use log::{debug, error, info, warn};
use tokio::io;
use tokio::net::UdpSocket;
use tokio::select;
use tokio::sync::{Mutex, Notify, RwLock, Semaphore};
use tokio::time::{interval, Instant};

use crate::error::Error;
use crate::items::{message, Confirmations, FileDetail, Manifest, Message, StartIndex};
use crate::sender::reader::reader;
use crate::{
    hash_file, socket_factory, CipherStream, Options, Result, TransferStats, ID_SIZE, INDEX_SIZE,
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
    stats: &TransferStats,
    rts_stream: CipherStream,
    mut str_stream: CipherStream,
    remote_addr: IpAddr,
    cancel_signal: Arc<Notify>,
) -> Result<()> {
    info!("sending {} -> {}", options.source, options.destination);

    let mut manifest = build_manifest(&options, &stats.total_data).await?;

    if manifest.is_empty() {
        warn!("found no files to send");
        str_stream.write_message(&Message::done(0)).await?;
        return Ok(());
    }

    debug!("sending manifest");
    str_stream
        .write_message(&Message::manifest(&manifest))
        .await?;

    let message: Message = str_stream.read_message().await?;

    match message.message {
        Some(message::Message::Completed(completed)) => {
            debug!("received {} completed ids", completed.ids.len());

            for id in completed.ids {
                if let Some(details) = manifest.files.remove(&id) {
                    stats.total_data.fetch_sub(details.size as usize, Relaxed);
                }
            }

            if manifest.files.is_empty() {
                info!("all files completed");
                str_stream.write_message(&Message::done(1)).await?;
                return Ok(());
            }
        }
        Some(message::Message::Failure(failure)) => {
            error!("received failure message {}", failure.reason);
            return Err(Error::failure(failure.reason));
        }
        _ => return Err(Error::unexpected_message(Box::new(message))),
    }

    let sockets = socket_factory(
        options.start_port,
        options.end_port,
        remote_addr,
        options.threads,
    )
    .await?;

    info!("opened sockets");

    // the reader fills the queue to `options.sender_limit` jobs, the unlimited capacity allows unconfirmed jobs to be added instantly
    let (job_sender, job_receiver) = kanal::unbounded_async();
    // a cache for the file chunks that have been sent but not confirmed
    let cache: JobCache = Default::default();

    // a semaphore to control the send rate
    let send = Arc::new(Semaphore::new(0));
    // a semaphore to control the readers
    let read = Arc::new(Semaphore::new(options.job_limit));

    // just confirmation messages
    let (confirmation_sender, confirmation_receiver) = kanal::unbounded_async();
    // end and failure messages
    let (controller_sender, controller_receiver) = kanal::unbounded_async();

    // receive messages from the receiver into two channels based on message type
    let receiver_handle = tokio::spawn(split_receiver(
        rts_stream,
        confirmation_sender,
        controller_sender,
    ));

    let confirmation_handle = tokio::spawn(receive_confirmations(
        confirmation_receiver,
        cache.clone(),
        job_sender.clone(),
        stats.confirmed_packets.clone(),
        read.clone(),
        Duration::from_millis(options.requeue_interval),
    ));

    tokio::spawn(add_permits_at_rate(send.clone(), options.pps()));

    let handles: Vec<_> = sockets
        .into_iter()
        .map(|socket| {
            tokio::spawn(sender(
                job_receiver.clone(),
                job_sender.clone(),
                socket,
                cache.clone(),
                send.clone(),
                stats.sent_packets.clone(),
                options.max_retries,
            ))
        })
        .collect();

    let controller_handle = tokio::spawn(controller(
        options,
        str_stream,
        manifest.files,
        job_sender.clone(),
        read,
        stats.clone(),
        controller_receiver,
    ));

    let sender_future = async {
        for handle in handles {
            handle.await??; // propagate errors
        }

        Ok(())
    };

    // propagate the first error
    select! {
        result = confirmation_handle => { debug!("confirmation receiver exited: {:?}", result); result? },
        result = controller_handle => { debug!("controller exited: {:?}", result); result? },
        result = sender_future => { debug!("senders exited: {:?}", result); result },
        result = receiver_handle => { debug!("message receiver exited: {:?}", result); result? },
        _ = cancel_signal.notified() => { debug!("stop signal received"); Ok(()) }
    }
}

async fn sender(
    job_receiver: AsyncReceiver<Job>,
    job_sender: AsyncSender<Job>,
    socket: UdpSocket,
    cache: JobCache,
    send: Arc<Semaphore>,
    sent: Arc<AtomicUsize>,
    max_retries: usize,
) -> Result<()> {
    let mut retries = 0;

    while retries < max_retries {
        let permit = send.acquire().await?; // acquire a permit
        let mut job = job_receiver.recv().await?; // get a job from the queue

        // send the job data to the socket
        if let Err(error) = socket.send(&job.data).await {
            error!("send error: {}", error);
            job_sender.send(job).await?; // put the job back in the queue
            retries += 1;
        } else {
            // cache the job
            job.cached_at = Some(Instant::now());
            cache.write().await.insert((job.id, job.index), job);
            retries = 0;
            sent.fetch_add(1, Relaxed);
        }

        permit.forget();
    }

    if retries == max_retries {
        Err(Error::max_retries())
    } else {
        Ok(())
    }
}

async fn controller(
    options: Options,
    mut control_stream: CipherStream,
    mut files: HashMap<u32, FileDetail>,
    job_sender: AsyncSender<Job>,
    read: Arc<Semaphore>,
    stats: TransferStats,
    controller_receiver: AsyncReceiver<Message>,
) -> Result<()> {
    let mut id = 0;
    let mut active: HashMap<u32, FileDetail> = HashMap::with_capacity(options.max);

    loop {
        while active.len() < options.max && !files.is_empty() {
            match files.remove(&id) {
                None => id += 1,
                Some(details) => {
                    start_file_transfer(
                        &mut control_stream,
                        id,
                        &details,
                        &options.source.file_path,
                        &job_sender,
                        &read,
                        &stats.total_data,
                    )
                    .await?;

                    active.insert(id, details);
                    id += 1
                }
            }
        }

        debug!("waiting for a message");

        let message: Message = controller_receiver.recv().await?;

        match message.message {
            Some(message::Message::End(end)) => {
                if active.remove(&end.id).is_none() {
                    warn!("received end message for unknown file {}", end.id);
                } else {
                    debug!("received end message {} | active {}", end.id, active.len());
                }
            }
            Some(message::Message::Failure(failure)) if failure.reason == 0 => {
                if let Some(details) = active.get(&failure.id) {
                    warn!(
                        "transfer {} failed signature verification, retrying...",
                        failure.id
                    );

                    stats.total_data.fetch_add(details.size as usize, Relaxed);

                    start_file_transfer(
                        &mut control_stream,
                        failure.id,
                        details,
                        &options.source.file_path,
                        &job_sender,
                        &read,
                        &stats.total_data,
                    )
                    .await?;
                } else {
                    warn!("received failure message {:?} for unknown file", failure);
                }
            }
            Some(message::Message::Failure(failure)) if failure.reason == 2 => {
                if active.remove(&failure.id).is_some() {
                    error!(
                        "remote writer failed {} [TRANSFER WILL NOT BE RETRIED]",
                        failure.description
                    );
                } else {
                    warn!(
                        "received writer failure message {:?} for unknown file",
                        failure
                    );
                }
            }
            Some(message::Message::Failure(failure)) => {
                warn!("received unknown failure message {:?}", failure);
            }
            _ => return Err(Error::unexpected_message(Box::new(message))),
        }

        if files.is_empty() && active.is_empty() {
            break;
        }
    }

    debug!("all files completed, sending done message");
    control_stream.write_message(&Message::done(2)).await?;

    Ok(())
}

async fn start_file_transfer(
    control_stream: &mut CipherStream,
    id: u32,
    details: &FileDetail,
    base_path: &Path,
    job_sender: &AsyncSender<Job>,
    read: &Arc<Semaphore>,
    total_data: &Arc<AtomicUsize>,
) -> Result<()> {
    control_stream.write_message(&Message::start(id)).await?;

    let start_index: StartIndex = control_stream.read_message().await?;
    total_data.fetch_sub(start_index.index as usize, Relaxed);

    let cipher = details.crypto.as_ref().unwrap().make_cipher()?;

    tokio::spawn({
        let job_sender = job_sender.clone();
        let read = read.clone();
        let details = details.clone();
        let base_path = base_path.to_path_buf();

        let path = if base_path.is_dir() {
            base_path.join(details.path)
        } else {
            base_path
        };

        async move {
            let result = reader(path, job_sender, read, start_index.index, id, cipher).await;

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
    confirmed_packets: Arc<AtomicUsize>,
    read: Arc<Semaphore>,
    requeue_interval: Duration,
) -> Result<()> {
    // this solves a problem where a confirmation is received after a job has already been requeued
    let lost_confirmations: Arc<Mutex<HashSet<(u32, u64)>>> = Default::default();

    // this thread checks the cache for unconfirmed jobs that have been there for too long and requeues them
    let requeue_handle = tokio::spawn({
        let cache = cache.clone();
        let lost_confirmations = lost_confirmations.clone();
        let confirmed_packets = confirmed_packets.clone();
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
                        unconfirmed.cached_at.unwrap().elapsed() > requeue_interval
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
                            confirmed_packets.fetch_add(1, Relaxed);
                        } else {
                            unconfirmed.cached_at = None;
                            job_sender.send(unconfirmed).await?;
                        }
                    }
                }
            }
        }
    });

    let future = async {
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
                        confirmed_packets.fetch_add(1, Relaxed);
                    }
                }
            }
        }
    };

    select! {
        result = requeue_handle => { debug!("requeue thread exited: {:?}", result); result? },
        _ = future => { debug!("confirmation receiver exited"); Ok(()) },
    }
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

/// split the message stream into `Confirmation` and `End + Failure` messages
async fn split_receiver(
    mut stream: CipherStream,
    confirmation_sender: AsyncSender<Confirmations>,
    controller_sender: AsyncSender<Message>,
) -> Result<()> {
    loop {
        let message: Message = stream.read_message().await?;

        match message.message {
            Some(message::Message::Confirmations(confirmations)) => {
                confirmation_sender.send(confirmations).await?
            }
            Some(message::Message::End(_)) => controller_sender.send(message).await?,
            Some(message::Message::Failure(_)) => controller_sender.send(message).await?,
            _ => return Err(Error::unexpected_message(Box::new(message))),
        }
    }
}

/// builds a manifest of the files to send and some details about them
async fn build_manifest(options: &Options, total_data: &Arc<AtomicUsize>) -> Result<Manifest> {
    // collect the files and directories to send
    let mut files = Vec::new();
    let mut dirs = Vec::new();

    files_and_dirs(
        &options.source.file_path,
        &mut files,
        &mut dirs,
        options.recursive,
    )?;

    debug!("found {} files & {} dirs", files.len(), dirs.len());

    let file_map: HashMap<u32, FileDetail> = iter(files.into_iter().enumerate())
        .map(|(index, mut file)| async move {
            let size = tokio::fs::metadata(&file).await?.len();
            total_data.fetch_add(size as usize, Relaxed);

            let signature = if options.verify {
                let hash = hash_file(&file).await?;
                hash.as_bytes().to_vec()
            } else {
                Vec::new()
            };

            if file == options.source.file_path {
                file = PathBuf::from(file.iter().last().ok_or(Error::empty_path())?);
            } else {
                file = file.strip_prefix(&options.source.file_path)?.to_path_buf();
            }

            let mut crypto = options.stream_crypto.clone();
            crypto.random_iv();

            Ok::<(u32, FileDetail), Error>((
                index as u32,
                FileDetail {
                    path: format_dir(file.to_string_lossy()),
                    size,
                    signature,
                    crypto: Some(crypto),
                },
            ))
        })
        .buffer_unordered(options.threads)
        .try_collect()
        .await?;

    let directories = dirs
        .into_iter()
        .map(|dir| {
            if let Ok(file_path) = dir.strip_prefix(&options.source.file_path) {
                format_dir(file_path.to_string_lossy())
            } else {
                format_dir(dir.to_string_lossy())
            }
        })
        .collect();

    let manifest = Manifest {
        directories,
        files: file_map,
    };

    Ok(manifest)
}

/// collect all files and directories in a directory
fn files_and_dirs(
    path: &Path,
    files: &mut Vec<PathBuf>,
    dirs: &mut Vec<PathBuf>,
    recursive: bool,
) -> io::Result<()> {
    if !path.exists() {
        warn!("dir crawler found a path that does not exist: {:?}", path);
        return Ok(());
    }

    if path.is_dir() {
        for entry in path.read_dir()?.filter_map(std::result::Result::ok) {
            let path = entry.path();

            if path.is_dir() {
                if recursive {
                    dirs.push(path.clone());
                    files_and_dirs(&path, files, dirs, recursive)?;
                }
            } else {
                files.push(path);
            }
        }
    } else {
        files.push(path.to_path_buf());
    }

    Ok(())
}

#[cfg(windows)]
#[inline(always)]
fn format_dir(dir: Cow<'_, str>) -> String {
    dir.replace('\\', "/") // replace the windows path separator with the unix one
}

#[cfg(not(windows))]
#[inline(always)]
fn format_dir(dir: Cow<'_, str>) -> String {
    dir.to_string()
}
