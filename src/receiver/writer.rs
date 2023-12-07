use async_channel::{SendError, Sender};
use deadqueue::limited::Queue;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::Arc;

use log::{debug, info};
use tokio::fs::{rename, OpenOptions};
use tokio::io::{self, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::sync::RwLock;

use crate::items::{message, End, Message};
use crate::receiver::{ConfirmationQueue, Job, WriterQueue};
use crate::{TRANSFER_BUFFER_SIZE, WRITE_BUFFER_SIZE};

#[derive(Default)]
pub(crate) struct SplitQueue {
    inner: RwLock<HashMap<u32, Arc<Queue<Job>>>>,
}

impl SplitQueue {
    pub(crate) async fn push_queue(&self, id: u32) {
        let mut inner = self.inner.write().await;

        inner.insert(id, Arc::new(Queue::new(1_000)));
    }

    pub(crate) async fn pop_queue(&self, id: &u32) {
        let mut inner = self.inner.write().await;

        inner.remove(id);
    }

    pub(crate) async fn push(&self, job: Job, id: u32) {
        let inner = self.inner.read().await;

        if let Some(queue) = inner.get(&id) {
            queue.push(job).await;
        }
    }

    pub(crate) async fn pop(&self, id: &u32) -> Option<Job> {
        let queue = {
            let inner = self.inner.read().await;
            inner.get(id).cloned()
        };

        if let Some(queue) = queue {
            Some(queue.pop().await)
        } else {
            None
        }
    }
}

/// stores file details for writer
pub(crate) struct File {
    pub(crate) path: PathBuf,
    pub(crate) partial_path: PathBuf,
    pub(crate) file_size: u64,
}

impl File {
    /// rename the partial file to the final name
    async fn rename(&self) -> io::Result<()> {
        rename(&self.partial_path, &self.path).await
    }
}

pub(crate) async fn writer(
    file_details: File,
    writer_queue: WriterQueue,
    confirmation_queue: ConfirmationQueue,
    mut position: u64,
    id: u32,
    message_sender: Sender<Message>,
) -> io::Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .open(&file_details.partial_path)
        .await?;

    let mut writer = BufWriter::with_capacity(WRITE_BUFFER_SIZE, file);
    writer.seek(SeekFrom::Start(position)).await?; // seek to the initial position

    debug!(
        "writer for {} starting at {}",
        file_details.path.display(),
        position
    );

    let mut cache: BTreeMap<u64, Job> = BTreeMap::new();

    while position != file_details.file_size {
        let job = writer_queue.pop(&id).await.unwrap();

        match job.index.cmp(&position) {
            // if the chunk is behind the current position, it was already written
            Ordering::Less => continue,
            // if the chunk is ahead of the current position, save it for later
            Ordering::Greater => {
                confirmation_queue.push((id, job.index));
                cache.insert(job.index, job);
                continue;
            }
            // if the chunk is at the current position, write it
            Ordering::Equal => {
                write_data(
                    &mut writer,
                    &job.data,
                    &mut position,
                    file_details.file_size,
                )
                .await?;
                confirmation_queue.push((id, job.index));
            }
        }

        // write all concurrent chunks from the cache
        while let Some(job) = cache.remove(&position) {
            write_data(
                &mut writer,
                &job.data,
                &mut position,
                file_details.file_size,
            )
            .await?;
        }
    }

    info!("writer wrote all expected bytes");

    writer.flush().await?; // flush the writer
    file_details.rename().await?; // rename the file
    writer_queue.pop_queue(&id).await; // remove the queue
    send_end_message(&message_sender, id)
        .await
        .expect("failed to send end message"); // send end message

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

async fn send_end_message(sender: &Sender<Message>, id: u32) -> Result<(), SendError<Message>> {
    let end_message = Message {
        message: Some(message::Message::End(End { id })),
    };

    sender.send(end_message).await
}
