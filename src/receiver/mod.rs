use std::collections::HashMap;
use std::env::current_dir;
use std::mem;
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
use tokio::fs::{create_dir, metadata};
use tokio::net::UdpSocket;
use tokio::select;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout};

use crate::error::Error;
use crate::items::{message, ConfirmationIndexes, Message, StartIndex};
use crate::receiver::writer::{writer, FileDetails, SplitQueue};
use crate::{
    socket_factory, CipherStream, Options, Result, TransferStats, ID_SIZE, INDEX_SIZE,
    TRANSFER_BUFFER_SIZE,
};

mod writer;

type WriterQueue = Arc<SplitQueue>;

#[derive(Clone)]
struct Job {
    data: [u8; TRANSFER_BUFFER_SIZE], // the file chunk
    index: u64,                       // the index of the file chunk
}

pub(crate) async fn main(
    options: Options,
    stats: &TransferStats,
    rts_stream: CipherStream,
    mut str_stream: CipherStream,
    remote_addr: IpAddr,
    cancel_signal: Arc<Notify>,
) -> Result<()> {
    info!("receiving {} -> {}", options.source, options.destination);

    let message: Message = str_stream.read_message().await?;

    let manifest = match message.message {
        Some(message::Message::Manifest(manifest)) => {
            debug!(
                "received manifest | files={} dirs={}",
                manifest.files.len(),
                manifest.directories.len()
            );

            manifest
        }
        Some(message::Message::Done(done)) if done.reason == 0 => {
            warn!("remote client found no files to send");
            return Ok(());
        }
        _ => return Err(Error::unexpected_message(Box::new(message))),
    };

    // if multiple files are being received, the destination should be a directory
    let is_dir = options.directory || manifest.files.len() > 1;
    let mut completed = Vec::new();

    let filtered_files = manifest.files.into_iter().filter_map(|(id, details)| {
        // formats the path to the file locally
        let path = if is_dir || options.destination.file_path.is_dir() {
            options.destination.file_path.join(&details.path)
        } else {
            options.destination.file_path.clone()
        };

        debug!("formatted local path {:?} for {:?}", path, details.path);

        if path.exists() && !options.overwrite {
            completed.push(id);
            None
        } else {
            Some((id, details, path))
        }
    });

    let files: HashMap<u32, FileDetails> = iter(filtered_files)
        .map(|(id, details, path)| {
            let total_data = stats.total_data.clone();

            async move {
                // append partial extension to the existing extension, if there is one
                let partial_extension = if let Some(extension) = path.extension() {
                    extension
                        .to_str()
                        .ok_or(Error::invalid_extension())?
                        .to_owned()
                        + ".partial"
                } else {
                    "partial".to_string()
                };

                let partial_path = path.with_extension(partial_extension);
                let start_index = start_index(&partial_path).await?;

                // increment the total data counter
                total_data.fetch_add((details.size - start_index) as usize, Relaxed);

                Ok::<(u32, FileDetails), Error>((
                    id,
                    FileDetails {
                        id,
                        path,
                        partial_path,
                        size: details.size,
                        start_index,
                        signature: details.signature,
                        crypto: details.crypto.unwrap_or_default(),
                    },
                ))
            }
        })
        .buffer_unordered(options.threads)
        .try_collect()
        .await?;

    debug!(
        "processed files | files={} completed={}",
        files.len(),
        completed.len()
    );

    if !options.force {
        let free_space = free_space(&options.destination.file_path)?;
        debug!("free space: {}", free_space);

        if free_space < stats.total_data.load(Relaxed) as u64 {
            error!(
                "not enough free space {} / {}",
                free_space,
                stats.total_data.load(Relaxed)
            );

            str_stream
                .write_message(&Message::failure(0, 1, None))
                .await?;

            return Err(Error::failure(1));
        }
    }

    debug!("sending completed: {:?}", completed);
    // send the completed message to the remote client
    str_stream
        .write_message(&Message::completed(completed))
        .await?;

    // if the destination is a directory, create it
    if is_dir {
        if !options.destination.file_path.exists() {
            debug!("creating directory {:?}", options.destination.file_path);
            create_dir(&options.destination.file_path).await?;
        }

        // create the local directories needed to write the files
        for dir in &manifest.directories {
            let local_dir = options.destination.file_path.join(dir);

            if !local_dir.exists() {
                debug!("creating directory {:?}", local_dir);
                create_dir(local_dir).await?;
            }
        }
    }

    let sockets = socket_factory(
        options.start_port,
        options.end_port,
        remote_addr,
        options.threads,
    )
    .await?;

    info!("opened sockets");

    let writer_queue: WriterQueue = Default::default();
    let (confirmation_sender, confirmation_receiver) = kanal::unbounded_async();

    // `message_sender` can now be used to send messages to the sender
    let (message_sender, message_receiver) = kanal::unbounded_async();
    let message_sender_handle = tokio::spawn(send_messages(rts_stream, message_receiver));

    let confirmation_handle = tokio::spawn(send_confirmations(
        message_sender.clone(),
        confirmation_receiver,
        stats.confirmed_packets.clone(),
    ));

    let controller_handle = tokio::spawn(controller(
        str_stream,
        files,
        writer_queue.clone(),
        confirmation_sender,
        message_sender,
    ));

    let receive_timeout = Duration::from_millis(options.receive_timeout);

    let handles: Vec<_> = sockets
        .into_iter()
        .map(|socket| {
            tokio::spawn(receiver(
                writer_queue.clone(),
                socket,
                receive_timeout,
                options.max_retries,
            ))
        })
        .collect();

    let receiver_future = async {
        for handle in handles {
            handle.await??;
        }

        Ok(())
    };

    select! {
        result = confirmation_handle => { debug!("confirmation sender exited: {:?}", result); result? },
        result = controller_handle => { debug!("controller exited: {:?}", result); result? },
        result = receiver_future => { debug!("receivers exited: {:?}", result); result },
        result = message_sender_handle => { debug!("message sender exited: {:?}", result); result? },
        _ = cancel_signal.notified() => { debug!("stop signal received"); Ok(()) }
    }
}

async fn receiver(
    queue: WriterQueue,
    socket: UdpSocket,
    receive_timeout: Duration,
    max_retries: usize,
) -> Result<()> {
    let mut buf = [0; ID_SIZE + INDEX_SIZE + TRANSFER_BUFFER_SIZE]; // buffer for receiving data
    let mut retries = 0; // counter to keep track of retries

    while retries < max_retries {
        match timeout(receive_timeout, socket.recv(&mut buf)).await {
            Ok(Ok(read)) if read > 0 => {
                retries = 0; // reset retries

                // slice the buffer into the id, index, and data
                let id = u32::from_be_bytes(buf[..ID_SIZE].try_into()?);
                let index = u64::from_be_bytes(buf[ID_SIZE..INDEX_SIZE + ID_SIZE].try_into()?);
                let data = buf[INDEX_SIZE + ID_SIZE..].try_into()?;

                if queue.send(Job { data, index }, id).await.is_err() {
                    // a message was received for a file that has already been completed (probably)
                    debug!("failed to send job for {} to writer", id);
                }
            }
            Ok(Ok(_)) => warn!("0 byte read?"), // this should never happen
            Ok(Err(error)) => {
                retries += 1;
                error!("recv error: {:?}", error);
            }
            Err(timeout) => {
                retries += 1;
                error!("recv timeout: {:?}", timeout);
            }
        }
    }

    if retries == max_retries {
        Err(Error::max_retries())
    } else {
        Ok(())
    }
}

async fn controller(
    mut control_stream: CipherStream,
    mut files: HashMap<u32, FileDetails>,
    writer_queue: WriterQueue,
    confirmation_sender: AsyncSender<(u32, u64)>,
    message_sender: AsyncSender<Message>,
) -> Result<()> {
    loop {
        debug!("waiting for message");
        let message: Message = control_stream.read_message().await?;

        match message.message {
            Some(message::Message::Start(message)) => {
                debug!("received start message: {:?}", message);

                let details = match files.get_mut(&message.id) {
                    Some(details) => details,
                    None => {
                        error!("received start message for unknown id {}", message.id);
                        continue;
                    }
                };

                // this handles an edge case where a partially transferred file fails and needs to be retried from the start
                if details.start_index > 0 && !details.partial_path.exists() {
                    details.start_index = 0;
                }

                // send the start index to the remote client
                control_stream
                    .write_message(&StartIndex::new(details.start_index))
                    .await?;

                writer_queue.push_queue(message.id).await; // create a queue for the writer

                tokio::spawn({
                    let queue = writer_queue.clone();
                    let confirmation = confirmation_sender.clone();
                    let message = message_sender.clone();
                    let details = details.clone();

                    async move {
                        let result = writer(&details, queue, confirmation, &message).await;

                        if let Err(error) = result {
                            message
                                .send(Message::failure(details.id, 2, Some(error.to_string())))
                                .await
                                .unwrap();
                            error!("writer failed: {:?}", error);
                        }
                    }
                });
            }
            Some(message::Message::Done(message)) => {
                match message.reason {
                    // 0 => warn!("remote client found no files to send"),
                    1 => debug!("all transfers were completed before execution"),
                    2 => debug!("remote client completed all transfers"),
                    _ => break Err(Error::unexpected_message(Box::new(message))),
                }

                message_sender.close();
                break Ok(());
            }
            _ => return Err(Error::unexpected_message(Box::new(message))),
        }
    }
}

async fn send_confirmations(
    sender: AsyncSender<Message>,
    confirmation_receiver: AsyncReceiver<(u32, u64)>,
    confirmed_packets: Arc<AtomicUsize>,
) -> Result<()> {
    let data: Arc<Mutex<Vec<(u32, u64)>>> = Default::default();

    let sender_handle: JoinHandle<Result<()>> = tokio::spawn({
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
                let confirmations = mem::take(&mut *data);
                drop(data); // release the lock on data

                // group the confirmations by id
                let map: HashMap<u32, ConfirmationIndexes> =
                    confirmations
                        .into_iter()
                        .fold(HashMap::new(), |mut map, (id, index)| {
                            map.entry(id).or_default().inner.push(index);
                            map
                        });

                sender.send(Message::confirmations(map)).await?;
            }
        }
    });

    let future = async {
        while let Ok(confirmation) = confirmation_receiver.recv().await {
            confirmed_packets.fetch_add(1, Relaxed); // increment the confirmed counter
            data.lock().await.push(confirmation); // push the index to the data vector
        }
    };

    // propagate errors from the sender thread while executing the future
    select! {
        result = sender_handle => { debug!("confirmation sender exited: {:?}", result); result? },
        _ = future => Ok(())
    }
}

/// send messages from a channel to a cipher stream
async fn send_messages<M: prost::Message>(
    mut stream: CipherStream,
    receiver: AsyncReceiver<M>,
) -> Result<()> {
    while let Ok(message) = receiver.recv().await {
        stream.write_message(&message).await?;
    }

    Ok(())
}

/// returns the amount of free space in bytes for the given path
#[cfg(unix)]
fn free_space(path: &Path) -> Result<u64> {
    use nix::sys::statvfs::statvfs;

    let path = parent_path(path)?;
    debug!("getting free space for {:?}", path);
    let stat = statvfs(&path)?;

    Ok(stat.blocks_available() as u64 * stat.fragment_size() as u64)
}

/// returns the amount of free space in bytes for the given path
#[cfg(windows)]
fn free_space(path: &Path) -> Result<u64> {
    use widestring::U16CString;
    use windows_sys::Win32::Storage::FileSystem;

    let path = parent_path(path)?;
    let path = U16CString::from_os_str(path)?;

    let mut free_bytes = 0_u64;
    let mut total_bytes = 0_u64;

    let status = unsafe {
        FileSystem::GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut free_bytes,
            &mut total_bytes,
            std::ptr::null_mut(),
        )
    };

    if status == 0 {
        Err(Error::status_error())
    } else {
        Ok(free_bytes)
    }
}

/// returns the absolute path of the first existing parent directory
fn parent_path(path: &Path) -> Result<PathBuf> {
    let mut path = path.to_path_buf();

    if !path.is_absolute() {
        let working_dir = current_dir()?;
        path = working_dir.join(path);
    }

    while !path.exists() {
        path = path.parent().ok_or(Error::empty_path())?.to_path_buf();
    }

    Ok(path)
}

/// returns the start index of the file, if it exists
async fn start_index(path: &Path) -> Result<u64> {
    if path.exists() {
        let metadata = metadata(&path).await?;
        // the file is written sequentially, so we can calculate the start index by rounding down to the nearest multiple of the transfer buffer size
        let chunks = metadata.len().div_floor(TRANSFER_BUFFER_SIZE as u64);
        Ok(chunks * TRANSFER_BUFFER_SIZE as u64)
    } else {
        Ok(0)
    }
}
