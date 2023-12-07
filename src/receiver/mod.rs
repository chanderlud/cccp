use async_channel::Sender;
use std::collections::HashMap;
use std::mem;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info, warn};
use tokio::fs::{create_dir_all, metadata};
use tokio::net::{TcpStream, UdpSocket};
use tokio::select;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout};

use crate::items::{message, ConfirmationIndexes, Confirmations, Files, Message, StartIndex};
use crate::receiver::writer::{writer, SplitQueue};
use crate::{
    read_message, socket_factory, write_message, Options, Result, TransferStats, UnlimitedQueue,
    ID_SIZE, INDEX_SIZE, MAX_RETRIES, RECEIVE_TIMEOUT, TRANSFER_BUFFER_SIZE,
};

mod writer;

type WriterQueue = Arc<SplitQueue>;
type ConfirmationQueue = UnlimitedQueue<(u32, u64)>;

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

    let files: Files = read_message(&mut str_stream).await?;
    debug!("received files: {:?}", files);

    // create the local directories needed to write the files
    for dir in &files.directories {
        let local_dir = options.destination.file_path.join(dir);
        debug!("creating directory {:?}", local_dir);
        create_dir_all(local_dir).await?;
    }

    // set the total data to be received
    for details in files.files.values() {
        stats
            .total_data
            .fetch_add(details.file_size as usize, Relaxed);
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
    let confirmation_queue: ConfirmationQueue = Default::default();

    // `message_sender` can now be used to send messages to the sender
    let (message_sender, message_receiver) = async_channel::unbounded();
    tokio::spawn(crate::message_sender(rts_stream, message_receiver));

    let confirmation_handle = tokio::spawn(send_confirmations(
        message_sender.clone(),
        confirmation_queue.clone(),
        stats.confirmed_data.clone(),
    ));

    let controller_handle = tokio::spawn(controller(
        str_stream,
        files.clone(),
        writer_queue.clone(),
        confirmation_queue,
        stats.confirmed_data.clone(),
        options.destination.file_path,
        message_sender,
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
        result = confirmation_handle => result?,
        result = controller_handle => result?,
        _ = receiver_future => { warn!("receiver(s) exited"); Ok(()) },
    }


}

async fn receiver(queue: WriterQueue, socket: UdpSocket) {
    let mut buf = [0; ID_SIZE + INDEX_SIZE + TRANSFER_BUFFER_SIZE]; // buffer for receiving data
    let mut retries = 0; // counter to keep track of retries

    while retries < MAX_RETRIES {
        match timeout(RECEIVE_TIMEOUT, socket.recv(&mut buf)).await {
            Ok(Ok(read)) if read > 0 => {
                retries = 0; // reset retries

                let id = u32::from_be_bytes(buf[..ID_SIZE].try_into().unwrap());
                let index =
                    u64::from_be_bytes(buf[ID_SIZE..INDEX_SIZE + ID_SIZE].try_into().unwrap());
                let data = buf[INDEX_SIZE + ID_SIZE..].try_into().unwrap();

                queue.push(Job { data, index }, id).await;
            }
            Ok(Ok(_)) => warn!("0 byte read?"), // this should never happen
            Ok(Err(_)) | Err(_) => retries += 1, // catch errors and timeouts
        }
    }
}

async fn controller(
    mut str_stream: TcpStream,
    mut files: Files,
    writer_queue: WriterQueue,
    confirmation_queue: ConfirmationQueue,
    confirmed_data: Arc<AtomicUsize>,
    file_path: PathBuf,
    message_sender: Sender<Message>,
) -> Result<()> {
    loop {
        let message: Message = read_message(&mut str_stream).await?;

        match message.message {
            Some(message::Message::Start(message)) => {
                debug!("received start message: {:?}", message);

                let details = files.files.remove(&message.id).unwrap();

                writer_queue.push_queue(message.id).await; // create a queue for the writer

                let file_path = if file_path.is_dir() {
                    file_path.join(&details.file_path)
                } else {
                    file_path.clone()
                };

                let partial_path = file_path.with_extension("partial");

                let start_index = if partial_path.exists() {
                    info!("partial file exists, resuming transfer");
                    let metadata = metadata(&partial_path).await?;
                    // the file is written sequentially, so we can calculate the start index by rounding down to the nearest multiple of the transfer buffer size
                    metadata.len().div_floor(TRANSFER_BUFFER_SIZE as u64)
                        * TRANSFER_BUFFER_SIZE as u64
                } else {
                    0
                };

                confirmed_data.fetch_add(start_index as usize, Relaxed);

                // send the start index to the remote client
                write_message(&mut str_stream, &StartIndex { index: start_index }).await?;

                let file = writer::File {
                    file_size: details.file_size,
                    partial_path,
                    path: file_path,
                };

                // TODO the result of writer is lost
                tokio::spawn(writer(
                    file,
                    writer_queue.clone(),
                    confirmation_queue.clone(),
                    start_index,
                    message.id,
                    message_sender.clone(),
                ));

                debug!("started file {:?}", details);
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
    sender: Sender<Message>,
    queue: UnlimitedQueue<(u32, u64)>,
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

                let map: HashMap<u32, ConfirmationIndexes> = HashMap::new();

                // group the confirmations by id
                let map = confirmations.into_iter().fold(map, |mut map, (id, index)| {
                    map.entry(id).or_default().inner.push(index);
                    map
                });

                let message = Message {
                    message: Some(message::Message::Confirmations(Confirmations {
                        indexes: map,
                    })),
                };
                sender
                    .send(message)
                    .await
                    .expect("failed to send confirmations");
            }
        }
    });

    let future = async {
        loop {
            let confirmation = queue.pop().await; // wait for a confirmation
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
