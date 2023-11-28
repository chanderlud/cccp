use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::Arc;

use log::debug;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, BufReader, Result};
use tokio::sync::Semaphore;

use crate::sender::{Job, JobQueue};
use crate::{INDEX_SIZE, READ_BUFFER_SIZE, TRANSFER_BUFFER_SIZE};

pub(crate) async fn reader(
    path: PathBuf,
    queue: JobQueue,
    read: Arc<Semaphore>,
    mut index: u64,
) -> Result<()> {
    let file = File::open(path).await?;
    let mut reader = BufReader::with_capacity(READ_BUFFER_SIZE, file);

    let mut buffer = vec![0; INDEX_SIZE + TRANSFER_BUFFER_SIZE];

    if index > 0 {
        index += TRANSFER_BUFFER_SIZE as u64;
    }

    reader.seek(SeekFrom::Start(index)).await?;

    debug!("starting reader at index {}", index);

    loop {
        let permit = read.acquire().await.unwrap();

        // write index
        buffer[..INDEX_SIZE].copy_from_slice(&index.to_be_bytes());

        // read data into buffer after checksum and index
        let read = reader.read(&mut buffer[INDEX_SIZE..]).await?;

        // check if EOF
        if read == 0 {
            break;
        }

        // push job with index, checksum, and data
        queue.push(Job {
            data: buffer[..INDEX_SIZE + read].to_vec(),
            index,
            cached_at: None,
        });

        index += read as u64;
        permit.forget();
    }

    Ok(())
}
