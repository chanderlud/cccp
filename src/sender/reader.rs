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
    reader.seek(SeekFrom::Start(index)).await?;

    let mut buffer = [0; INDEX_SIZE + TRANSFER_BUFFER_SIZE];

    debug!("starting reader at index {}", index);

    loop {
        let permit = read.acquire().await.unwrap();

        // write index to buffer
        buffer[..INDEX_SIZE].copy_from_slice(&index.to_be_bytes());

        // read data into buffer after checksum and index
        let read = reader.read(&mut buffer[INDEX_SIZE..]).await?;

        // check if EOF
        if read == 0 {
            break;
        }

        // push job to queue
        queue.push(Job {
            data: buffer,
            index,
            cached_at: None,
        });

        index += read as u64; // increment index by bytes read
        permit.forget(); // release permit
    }

    Ok(())
}
