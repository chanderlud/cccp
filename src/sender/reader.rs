use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use log::debug;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader, Result};
use tokio::sync::Semaphore;

use crate::sender::{Job, JobQueue};
use crate::{INDEX_SIZE, READ_BUFFER_SIZE, TRANSFER_BUFFER_SIZE};

pub(crate) async fn reader(
    path: PathBuf,
    queue: JobQueue,
    read: Arc<Semaphore>,
    prior_indexes: HashSet<u64>,
) -> Result<()> {
    let file = File::open(path).await?;
    let mut reader = BufReader::with_capacity(READ_BUFFER_SIZE, file);
    let mut index = 0_u64;

    let mut buffer = vec![0; INDEX_SIZE + TRANSFER_BUFFER_SIZE];

    loop {
        if prior_indexes.contains(&index) {
            index += TRANSFER_BUFFER_SIZE as u64;
            continue;
        }

        let permit = read.acquire().await.unwrap();

        // write index
        buffer[..INDEX_SIZE].copy_from_slice(&index.to_be_bytes());

        // read data into buffer after checksum and index
        let read = reader.read(&mut buffer[INDEX_SIZE..]).await?;

        // check if EOF
        if read == 0 {
            break;
        } else if read != TRANSFER_BUFFER_SIZE {
            debug!("abnormal read {} bytes (should be last chunk)", read);
        }

        // push job with index, checksum, and data
        queue.push(Job {
            data: buffer[..INDEX_SIZE + read].to_vec(),
            index,
            cached_at: None,
            reader: true,
        });

        index += read as u64;
        permit.forget();
    }

    Ok(())
}
