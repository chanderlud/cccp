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
use tokio::io::AsyncWrite;
use tokio::net::{TcpStream, UdpSocket};
use tokio::select;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout};

use crate::error::Error;
use crate::items::{message, ConfirmationIndexes, Manifest, Message, StartIndex};
use crate::receiver::writer::{writer, FileDetails, SplitQueue};
use crate::{
    make_cipher, read_message, socket_factory, write_message, Options, Result, StreamCipherExt,
    TransferStats, ID_SIZE, INDEX_SIZE, MAX_RETRIES, RECEIVE_TIMEOUT, TRANSFER_BUFFER_SIZE,
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
    stats: TransferStats,
    rts_stream: TcpStream,
    mut str_stream: TcpStream,
    remote_addr: IpAddr,
) -> Result<()> {
    info!("receiving {} -> {}", options.source, options.destination);

    let mut str_cipher = make_cipher(&options.control_crypto);
    let rts_cipher = make_cipher(&options.control_crypto);

    let manifest: Manifest = read_message(&mut str_stream, &mut str_cipher).await?;
    let is_dir = manifest.files.len() > 1; // if multiple files are being received, the destination should be a directory
    debug!(
        "received manifest | files={} dirs={}",
        manifest.files.len(),
        manifest.directories.len()
    );

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
                        crypto: details.crypto,
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

    let free_space = free_space(&options.destination.file_path)?;
    debug!("free space: {}", free_space);

    if free_space < stats.total_data.load(Relaxed) as u64 {
        error!(
            "not enough free space {} / {}",
            free_space,
            stats.total_data.load(Relaxed)
        );

        write_message(
            &mut str_stream,
            &Message::failure(0, 1, None),
            &mut str_cipher,
        )
        .await?;

        return Err(Error::failure(1));
    }

    debug!("sending completed: {:?}", completed);
    // send the completed message to the remote client
    write_message(
        &mut str_stream,
        &Message::completed(completed),
        &mut str_cipher,
    )
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
        options.start_port + 2, // the first two ports are used for control messages and confirmations
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
    tokio::spawn(send_messages(rts_stream, message_receiver, rts_cipher));

    let confirmation_handle = tokio::spawn(send_confirmations(
        message_sender.clone(),
        confirmation_receiver,
        stats.confirmed_data,
    ));

    let controller_handle = tokio::spawn(controller(
        str_stream,
        files,
        writer_queue.clone(),
        confirmation_sender,
        message_sender,
        str_cipher,
    ));

    let handles: Vec<_> = sockets
        .into_iter()
        .map(|socket| tokio::spawn(receiver(writer_queue.clone(), socket)))
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
    }
}

async fn receiver(queue: WriterQueue, socket: UdpSocket) -> Result<()> {
    let mut buf = [0; ID_SIZE + INDEX_SIZE + TRANSFER_BUFFER_SIZE]; // buffer for receiving data
    let mut retries = 0; // counter to keep track of retries

    while retries < MAX_RETRIES {
        match timeout(RECEIVE_TIMEOUT, socket.recv(&mut buf)).await {
            Ok(Ok(read)) if read > 0 => {
                retries = 0; // reset retries

                // slice the buffer into the id, index, and data
                let id = u32::from_be_bytes(buf[..ID_SIZE].try_into()?);
                let index = u64::from_be_bytes(buf[ID_SIZE..INDEX_SIZE + ID_SIZE].try_into()?);
                let data = buf[INDEX_SIZE + ID_SIZE..].try_into()?;

                queue.send(Job { data, index }, id).await?;
            }
            Ok(Ok(_)) => warn!("0 byte read?"), // this should never happen
            Ok(Err(_)) | Err(_) => retries += 1, // catch errors and timeouts
        }
    }

    if retries == MAX_RETRIES {
        Err(Error::max_retries())
    } else {
        Ok(())
    }
}

async fn controller<C: StreamCipherExt + ?Sized>(
    mut str_stream: TcpStream,
    mut files: HashMap<u32, FileDetails>,
    writer_queue: WriterQueue,
    confirmation_sender: AsyncSender<(u32, u64)>,
    message_sender: AsyncSender<Message>,
    mut str_cipher: Box<C>,
) -> Result<()> {
    loop {
        debug!("waiting for message");
        let message: Message = read_message(&mut str_stream, &mut str_cipher).await?;

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
                write_message(
                    &mut str_stream,
                    &StartIndex::new(details.start_index),
                    &mut str_cipher,
                )
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
            Some(message::Message::Done(_)) => {
                debug!("received done message");
                message_sender.close();
                break Ok(());
            }
            _ => unreachable!("controller received unexpected message: {:?}", message),
        }
    }
}

async fn send_confirmations(
    sender: AsyncSender<Message>,
    confirmation_receiver: AsyncReceiver<(u32, u64)>,
    confirmed_data: Arc<AtomicUsize>,
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
            confirmed_data.fetch_add(TRANSFER_BUFFER_SIZE, Relaxed); // increment the confirmed data counter
            data.lock().await.push(confirmation); // push the index to the data vector
        }
    };

    // propagate errors from the sender thread while executing the future
    select! {
        result = sender_handle => { debug!("confirmation sender exited: {:?}", result); result? },
        _ = future => Ok(())
    }
}

/// send messages from a channel to a writer
async fn send_messages<W: AsyncWrite + Unpin, M: prost::Message, C: StreamCipherExt + ?Sized>(
    mut writer: W,
    receiver: AsyncReceiver<M>,
    mut cipher: Box<C>,
) -> Result<()> {
    while let Ok(message) = receiver.recv().await {
        write_message(&mut writer, &message, &mut cipher).await?;
    }

    Ok(())
}

#[cfg(unix)]
fn free_space(path: &Path) -> Result<u64> {
    use nix::sys::statvfs::statvfs;

    let path = format_path(path)?;
    debug!("getting free space for {:?}", path);
    let stat = statvfs(&path)?;

    Ok(stat.blocks_available() as u64 * stat.fragment_size())
}

#[cfg(windows)]
fn free_space(path: &Path) -> Result<u64> {
    use widestring::U16CString;
    use windows_sys::Win32::Storage::FileSystem;

    let path = format_path(path)?;
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
fn format_path(path: &Path) -> Result<PathBuf> {
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
