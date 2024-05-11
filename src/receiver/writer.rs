use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::PathBuf;

use kanal::{AsyncReceiver, AsyncSender};
use log::{debug, info};
use tokio::fs::{remove_file, rename, OpenOptions};
use tokio::io::{self, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;

use crate::cipher::StreamCipherWrapper;
use crate::error::ErrorKind;
use crate::items::{Crypto, Message};
use crate::receiver::{Job, WriterQueue};
use crate::{hash_file, Result, TRANSFER_BUFFER_SIZE, WRITE_BUFFER_SIZE};

#[derive(Default)]
pub(crate) struct SplitQueue {
    senders: Mutex<HashMap<u32, AsyncSender<Job>>>,
    receivers: Mutex<HashMap<u32, AsyncReceiver<Job>>>,
}

impl SplitQueue {
    pub(crate) async fn push_queue(&self, id: u32) {
        let (sender, receiver) = kanal::bounded_async(1_000);

        self.receivers.lock().await.insert(id, receiver);
        self.senders.lock().await.insert(id, sender);
    }

    pub(crate) async fn pop_queue(&self, id: &u32) {
        self.receivers.lock().await.remove(id);
        self.senders.lock().await.remove(id);
    }

    pub(crate) async fn get_receiver(&self, id: &u32) -> Option<AsyncReceiver<Job>> {
        let receivers = self.receivers.lock().await;
        receivers.get(id).cloned()
    }

    pub(crate) async fn send(&self, job: Job, id: u32) -> Result<()> {
        let sender = {
            let senders = self.senders.lock().await;
            senders.get(&id).ok_or(ErrorKind::MissingQueue)?.clone()
        };

        sender.send(job).await?;
        Ok(())
    }
}

/// stores file details for writer
#[derive(Clone)]
pub(crate) struct FileDetails {
    pub(crate) id: u32,
    pub(crate) path: PathBuf,
    pub(crate) partial_path: PathBuf,
    pub(crate) size: u64,
    pub(crate) start_index: u64,
    pub(crate) signature: Option<Vec<u8>>,
    pub(crate) crypto: Crypto,
}

impl FileDetails {
    /// rename the partial file to the final name
    async fn rename(&self) -> io::Result<()> {
        rename(&self.partial_path, &self.path).await
    }
}

pub(crate) async fn writer(
    details: &FileDetails,
    writer_queue: WriterQueue,
    confirmation_sender: AsyncSender<(u32, u64)>,
    message_sender: &AsyncSender<Message>,
) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .append(true)
        .open(&details.partial_path)
        .await?;

    let mut position = details.start_index;

    let mut writer = BufWriter::with_capacity(WRITE_BUFFER_SIZE, file);
    writer.seek(SeekFrom::Start(position)).await?; // seek to the initial position

    let mut cipher = details.crypto.make_cipher()?;
    cipher.seek(position);

    debug!(
        "writer for {} starting at {}",
        details.path.display(),
        position
    );

    let mut cache: HashMap<u64, Job> = HashMap::new();
    let receiver = writer_queue
        .get_receiver(&details.id)
        .await
        .ok_or(ErrorKind::MissingQueue)?;

    while position != details.size {
        let job = receiver.recv().await?;

        match job.index.cmp(&position) {
            // if the chunk is behind the current position, it was already written
            Ordering::Less => continue,
            // if the chunk is ahead of the current position, save it for later
            Ordering::Greater => {
                confirmation_sender.send((details.id, job.index)).await?;
                cache.insert(job.index, job);
                continue;
            }
            // if the chunk is at the current position, write it
            Ordering::Equal => {
                write_data(
                    &mut writer,
                    job.data,
                    &mut position,
                    details.size,
                    &mut cipher,
                )
                .await?;
                confirmation_sender.send((details.id, job.index)).await?;
            }
        }

        // write all concurrent chunks from the cache
        while let Some(job) = cache.remove(&position) {
            write_data(
                &mut writer,
                job.data,
                &mut position,
                details.size,
                &mut cipher,
            )
            .await?;
        }
    }

    info!("writer wrote all expected bytes");

    writer.flush().await?; // flush the writer
    writer_queue.pop_queue(&details.id).await; // remove the queue

    // verify the signature if provided by the sender
    if let Some(ref remote_signature) = details.signature {
        let local_hash = hash_file(&details.partial_path).await?; // hash the file

        if local_hash.as_bytes() != &remote_signature[..] {
            message_sender
                .send(Message::failure(details.id, 0, None))
                .await?; // notify the sender
            remove_file(&details.partial_path).await?; // remove the partial file
            return Err(ErrorKind::Failure(0).into());
        } else {
            info!("{:?} passed signature verification", details.path)
        }
    }

    details.rename().await?; // rename the file
    debug!("sending end message for {}", details.id);
    message_sender.send(Message::end(details.id)).await?; // send the end message

    Ok(())
}

/// write data and advance position
#[inline]
async fn write_data<T: AsyncWrite + Unpin, C: StreamCipherWrapper + ?Sized>(
    writer: &mut T,
    mut buffer: [u8; TRANSFER_BUFFER_SIZE],
    position: &mut u64,
    file_size: u64,
    cipher: &mut Box<C>,
) -> io::Result<()> {
    // calculate the length of the data to write
    let len = (file_size - *position).min(TRANSFER_BUFFER_SIZE as u64);

    cipher.apply_keystream(&mut buffer[..len as usize]); // apply the keystream

    *position += len; // advance the position
    writer.write_all(&buffer[..len as usize]).await // write the data
}
