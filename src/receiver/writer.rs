use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::PathBuf;

use async_channel::Sender;
use log::{debug, info};
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};

use crate::{ByteQueue, INDEX_SIZE, TRANSFER_BUFFER_SIZE, WRITE_BUFFER_SIZE};

pub(crate) async fn writer(
    path: PathBuf,
    queue: ByteQueue,
    file_size: u64,
    confirmation: Sender<u64>,
) -> std::io::Result<()> {
    let file = File::create(path).await?;
    let mut writer = BufWriter::with_capacity(WRITE_BUFFER_SIZE, file);

    let mut position = 0;
    let mut data_map: BTreeMap<u64, (usize, Vec<u8>)> = BTreeMap::new();

    loop {
        let buffer = queue.pop().await;

        // extract each section from the buffer
        let index_bytes = &buffer[..INDEX_SIZE];

        let index = u64::from_be_bytes(
            index_bytes
                .try_into()
                .expect("failed to convert index to u64"),
        );

        let i = file_size - index;

        let len = if i < TRANSFER_BUFFER_SIZE as u64 {
            debug!("received last chunk | len = {}", i);
            i as usize
        } else {
            TRANSFER_BUFFER_SIZE
        };

        confirmation.send(index).await.unwrap();

        match index.cmp(&position) {
            Ordering::Less => {
                // if the chunk is behind the current position, it was already written
                continue;
            }
            Ordering::Greater => {
                // if the chunk is ahead of the current position, save it for later
                data_map.insert(index, (len, buffer));
                continue;
            }
            Ordering::Equal => {}
        }

        write_data(&mut writer, &buffer, &mut position, len).await?;

        while let Some((len, buffer)) = data_map.remove(&position) {
            write_data(&mut writer, &buffer, &mut position, len).await?;
        }

        if position >= file_size {
            info!("writer wrote all expected bytes");
            break;
        }
    }

    writer.flush().await?; // flush the writer
    confirmation.close();  // close the confirmation channel

    Ok(())
}

#[inline]
async fn write_data(
    writer: &mut BufWriter<File>,
    buffer: &[u8],
    position: &mut u64,
    len: usize,
) -> tokio::io::Result<()> {
    let data = &buffer[INDEX_SIZE..INDEX_SIZE + len];
    *position += data.len() as u64;

    writer.write_all(data).await
}
