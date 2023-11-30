use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::SeekFrom;
use std::path::PathBuf;

use log::{debug, info};
use tokio::fs::OpenOptions;
use tokio::io::{self, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use crate::receiver::metadata::Metadata;
use crate::receiver::{Job, WriterQueue};
use crate::{UnlimitedQueue, TRANSFER_BUFFER_SIZE};

pub(crate) async fn writer(
    path: PathBuf,
    writer_queue: WriterQueue,
    file_size: u64,
    confirmation_queue: UnlimitedQueue<u64>,
    mut metadata: Metadata,
) -> io::Result<()> {
    let mut writer = OpenOptions::new()
        .write(true)
        .create(true)
        .open(path)
        .await?;

    let mut position = metadata.initial_index;

    if position > 0 {
        position += TRANSFER_BUFFER_SIZE as u64; // the chunk at index has already been written
        writer.seek(SeekFrom::Start(position)).await?; // seek to the position
    }

    debug!("starting writer at position {}", position);

    let mut cache: BTreeMap<u64, Job> = BTreeMap::new();

    while position != file_size {
        let job = writer_queue.pop().await;

        match job.index.cmp(&position) {
            // if the chunk is behind the current position, it was already written
            Ordering::Less => continue,
            // if the chunk is ahead of the current position, save it for later
            Ordering::Greater => {
                confirmation_queue.push(job.index);
                cache.insert(job.index, job);
                continue;
            }
            // if the chunk is at the current position, write it
            Ordering::Equal => {
                write_data(&mut writer, &job.data, &mut position, job.len).await?;
                confirmation_queue.push(job.index);
                metadata.complete(job.index).await?;
            }
        }

        // write all concurrent chunks from `cache`
        while let Some(job) = cache.remove(&position) {
            write_data(&mut writer, &job.data, &mut position, job.len).await?;
            metadata.complete(job.index).await?;
        }
    }

    info!("writer wrote all expected bytes");
    metadata.remove().await?; // remove the metadata file

    Ok(())
}

/// write data and advance position
#[inline]
async fn write_data<T: AsyncWrite + Unpin>(
    writer: &mut T,
    buffer: &[u8],
    position: &mut u64,
    len: usize,
) -> io::Result<()> {
    *position += len as u64; // advance the position
    writer.write_all(&buffer[..len]).await // write the data
}
