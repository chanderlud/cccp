use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::PathBuf;

use kanal::{AsyncReceiver, AsyncSender};
use log::{debug, info};
use tokio::fs::{rename, OpenOptions};
use tokio::io::{self, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::sync::RwLock;

use crate::items::{message, End, Message};
use crate::receiver::{Job, WriterQueue};
use crate::{Result, TRANSFER_BUFFER_SIZE, WRITE_BUFFER_SIZE};

#[derive(Default)]
pub(crate) struct SplitQueue {
    senders: RwLock<HashMap<u32, AsyncSender<Job>>>,
    receivers: RwLock<HashMap<u32, AsyncReceiver<Job>>>,
}

impl SplitQueue {
    pub(crate) async fn push_queue(&self, id: u32) {
        let (sender, receiver) = kanal::bounded_async(1_000);

        self.receivers.write().await.insert(id, receiver);
        self.senders.write().await.insert(id, sender);
    }

    pub(crate) async fn pop_queue(&self, id: &u32) {
        self.receivers.write().await.remove(id);
        self.senders.write().await.remove(id);
    }

    pub(crate) async fn send(&self, job: Job, id: u32) {
        let senders = self.senders.read().await;

        if let Some(sender) = senders.get(&id) {
            sender.send(job).await.unwrap();
        }
    }

    pub(crate) async fn recv(&self, id: &u32) -> Option<Job> {
        let receiver = {
            let inner = self.receivers.read().await;
            inner.get(id).cloned()
        };

        if let Some(receiver) = receiver {
            receiver.recv().await.ok()
        } else {
            None
        }
    }
}

/// stores file details for writer
pub(crate) struct FileDetails {
    pub(crate) path: PathBuf,
    pub(crate) partial_path: PathBuf,
    pub(crate) file_size: u64,
}

impl FileDetails {
    /// rename the partial file to the final name
    async fn rename(&self) -> io::Result<()> {
        rename(&self.partial_path, &self.path).await
    }
}

pub(crate) async fn writer(
    details: FileDetails,
    writer_queue: WriterQueue,
    confirmation_sender: AsyncSender<(u32, u64)>,
    mut position: u64,
    id: u32,
    message_sender: AsyncSender<Message>,
) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .open(&details.partial_path)
        .await?;

    let mut writer = BufWriter::with_capacity(WRITE_BUFFER_SIZE, file);
    writer.seek(SeekFrom::Start(position)).await?; // seek to the initial position

    debug!(
        "writer for {} starting at {}",
        details.path.display(),
        position
    );

    let mut cache: HashMap<u64, Job> = HashMap::new();

    while position != details.file_size {
        let job = writer_queue.recv(&id).await.unwrap();

        match job.index.cmp(&position) {
            // if the chunk is behind the current position, it was already written
            Ordering::Less => continue,
            // if the chunk is ahead of the current position, save it for later
            Ordering::Greater => {
                confirmation_sender.send((id, job.index)).await?;
                cache.insert(job.index, job);
                continue;
            }
            // if the chunk is at the current position, write it
            Ordering::Equal => {
                write_data(&mut writer, &job.data, &mut position, details.file_size).await?;
                confirmation_sender.send((id, job.index)).await?;
            }
        }

        // write all concurrent chunks from the cache
        while let Some(job) = cache.remove(&position) {
            write_data(&mut writer, &job.data, &mut position, details.file_size).await?;
        }
    }

    info!("writer wrote all expected bytes");

    writer.flush().await?; // flush the writer
    details.rename().await?; // rename the file
    writer_queue.pop_queue(&id).await; // remove the queue
    send_end_message(&message_sender, id).await?;

    Ok(())
}

/// write data and advance position
#[inline]
async fn write_data<T: AsyncWrite + Unpin>(
    writer: &mut T,
    buffer: &[u8],
    position: &mut u64,
    file_size: u64,
) -> io::Result<()> {
    // calculate the length of the data to write
    let len = (file_size - *position).min(TRANSFER_BUFFER_SIZE as u64);

    *position += len; // advance the position
    writer.write_all(&buffer[..len as usize]).await // write the data
}

async fn send_end_message(sender: &AsyncSender<Message>, id: u32) -> Result<()> {
    let end_message = Message {
        message: Some(message::Message::End(End { id })),
    };

    sender.send(end_message).await?;
    Ok(())
}
