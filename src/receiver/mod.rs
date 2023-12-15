use std::collections::HashMap;
use std::mem;
use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use kanal::{AsyncReceiver, AsyncSender};
use log::{debug, error, info, warn};
use tokio::fs::{create_dir_all, metadata};
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
    debug!("received manifest: {:?}", manifest);

    let mut completed = Vec::new();

    // TODO get start indexes here and never add them to total_data
    let files = manifest
        .files
        .into_iter()
        .filter_map(|(id, details)| {
            // formats the path to the file locally
            let path = if is_dir {
                options.destination.file_path.join(&details.path)
            } else {
                options.destination.file_path.clone()
            };

            if path.exists() && !options.overwrite {
                completed.push(id);
                None
            } else {
                // increment the total data counter
                stats.total_data.fetch_add(details.size as usize, Relaxed);

                // append partial extension to the existing extension, if there is one
                let partial_extension = if let Some(extension) = path.extension() {
                    extension.to_str()?.to_owned() + ".partial"
                } else {
                    ".partial".to_string()
                };

                let partial_path = path.with_extension(partial_extension);

                Some((
                    id,
                    FileDetails {
                        path,
                        partial_path,
                        size: details.size,
                        signature: details.signature,
                        crypto: details.crypto,
                    },
                ))
            }
        })
        .collect();

    let free_space = free_space(&options.destination.file_path)?;
    debug!("free space: {}", free_space);

    if free_space < stats.total_data.load(Relaxed) as u64 {
        error!(
            "not enough free space {} / {}",
            free_space,
            stats.total_data.load(Relaxed)
        );

        write_message(&mut str_stream, &Message::failure(0, 1), &mut str_cipher).await?;

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

    if is_dir {
        create_dir_all(&options.destination.file_path).await?;
    }

    // create the local directories needed to write the files
    for dir in &manifest.directories {
        let local_dir = options.destination.file_path.join(dir);
        debug!("creating directory {:?}", local_dir);
        create_dir_all(local_dir).await?;
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
        stats.confirmed_data.clone(),
    ));

    let controller_handle = tokio::spawn(controller(
        str_stream,
        files,
        writer_queue.clone(),
        confirmation_sender,
        stats.confirmed_data,
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
        result = confirmation_handle => result?,
        result = controller_handle => result?,
        result = receiver_future => result,
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
    confirmed_data: Arc<AtomicUsize>,
    message_sender: AsyncSender<Message>,
    mut str_cipher: Box<C>,
) -> Result<()> {
    loop {
        let message: Message = read_message(&mut str_stream, &mut str_cipher).await?;

        match message.message {
            Some(message::Message::Start(message)) => {
                debug!("received start message: {:?}", message);

                let details = files.remove(&message.id).unwrap();

                let start_index = if details.partial_path.exists() {
                    info!("partial file exists, resuming transfer");
                    let metadata = metadata(&details.partial_path).await?;
                    // the file is written sequentially, so we can calculate the start index by rounding down to the nearest multiple of the transfer buffer size
                    let chunks = metadata.len().div_floor(TRANSFER_BUFFER_SIZE as u64);
                    chunks * TRANSFER_BUFFER_SIZE as u64
                } else {
                    0
                };

                // send the start index to the remote client
                write_message(
                    &mut str_stream,
                    &StartIndex::new(start_index),
                    &mut str_cipher,
                )
                .await?;

                writer_queue.push_queue(message.id).await; // create a queue for the writer
                confirmed_data.fetch_add(start_index as usize, Relaxed);

                tokio::spawn({
                    let writer_queue = writer_queue.clone();
                    let confirmation_sender = confirmation_sender.clone();
                    let message_sender = message_sender.clone();

                    async move {
                        let result = writer::<C>(
                            details,
                            writer_queue,
                            confirmation_sender,
                            start_index,
                            message.id,
                            message_sender,
                        )
                        .await;

                        if let Err(error) = result {
                            error!("writer failed: {:?}", error);
                        }
                    }
                });
            }
            Some(message::Message::Done(_)) => {
                debug!("received done message");
                message_sender.close();
                break;
            }
            _ => {
                error!("received {:?}", message);
                break;
            }
        }
    }

    Ok(())
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
        result = sender_handle => result?,
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

// TODO this is buggy af
#[cfg(unix)]
fn free_space(path: &Path) -> Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let dir = CString::new(path.as_os_str().as_bytes())?;

    unsafe {
        let mut buf: mem::MaybeUninit<libc::statvfs> = mem::MaybeUninit::uninit();
        let result = libc::statvfs(dir.as_ptr(), buf.as_mut_ptr());

        if result == 0 {
            let stat = buf.assume_init();
            Ok(stat.f_frsize as u64 * stat.f_bavail as u64)
        } else {
            Err(Error::status_error())
        }
    }
}

#[cfg(windows)]
fn free_space(path: &Path) -> Result<u64> {
    use widestring::U16CString;
    use windows_sys::Win32::Storage::FileSystem;

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
