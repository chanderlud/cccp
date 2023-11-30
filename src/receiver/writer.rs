use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::SeekFrom;
use std::path::PathBuf;

use log::{debug, info};
use tokio::fs::OpenOptions;
use tokio::io::{self, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use crate::receiver::metadata::Metadata;
use crate::receiver::WriterQueue;
use crate::{UnlimitedQueue, INDEX_SIZE, TRANSFER_BUFFER_SIZE};

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

    let mut data_map: BTreeMap<u64, (u64, usize, Vec<u8>)> = BTreeMap::new();

    loop {
        let buffer = writer_queue.pop().await;

        // extract each section from the buffer
        let index_bytes = &buffer[..INDEX_SIZE];

        // convert the index bytes to u64
        let index = u64::from_be_bytes(
            index_bytes
                .try_into()
                .expect("failed to convert index to u64"),
        );

        // calculate the distance between the index and the end of the file
        let delta = (file_size - index) as usize;

        let len = if delta < TRANSFER_BUFFER_SIZE {
            delta // only the last chunk will have a delta smaller than TRANSFER_BUFFER_SIZE
        } else {
            TRANSFER_BUFFER_SIZE
        };

        match index.cmp(&position) {
            // if the chunk is behind the current position, it was already written
            Ordering::Less => continue,
            // if the chunk is ahead of the current position, save it for later
            Ordering::Greater => {
                data_map.insert(index, (index, len, buffer));
                confirmation_queue.push(index);
                continue;
            }
            // if the chunk is at the current position, write it
            Ordering::Equal => {
                write_data(&mut writer, &buffer, &mut position, len).await?;
                confirmation_queue.push(index);
                metadata.complete(index).await?;
            }
        }

        // write all concurrent chunks from `data_map`
        while let Some((index, len, buffer)) = data_map.remove(&position) {
            write_data(&mut writer, &buffer, &mut position, len).await?;
            metadata.complete(index).await?;
        }

        if position == file_size {
            info!("writer wrote all expected bytes");
            break;
        }
    }

    writer.flush().await?; // flush the writer
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
    let data = &buffer[INDEX_SIZE..INDEX_SIZE + len]; // extract the data from the buffer
    *position += data.len() as u64; // advance the position

    writer.write_all(data).await // write the data
}
