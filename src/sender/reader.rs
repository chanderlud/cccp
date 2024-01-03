use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::Arc;

use kanal::AsyncSender;
use log::debug;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, BufReader};
use tokio::sync::Semaphore;

use crate::cipher::StreamCipherWrapper;
use crate::sender::Job;
use crate::{Result, ID_SIZE, INDEX_SIZE, READ_BUFFER_SIZE, TRANSFER_BUFFER_SIZE};

pub(crate) async fn reader<C: StreamCipherWrapper + ?Sized>(
    path: PathBuf,
    queue: AsyncSender<Job>,
    read: Arc<Semaphore>,
    mut index: u64,
    id: u32,
    mut cipher: Box<C>,
) -> Result<()> {
    let file = File::open(path).await?;
    let mut reader = BufReader::with_capacity(READ_BUFFER_SIZE, file);
    reader.seek(SeekFrom::Start(index)).await?;

    cipher.seek(index);

    let mut buffer = [0; ID_SIZE + INDEX_SIZE + TRANSFER_BUFFER_SIZE];
    // write id to buffer it is constant for all chunks
    buffer[..ID_SIZE].copy_from_slice(&id.to_be_bytes());

    debug!("starting reader at index {}", index);

    loop {
        let permit = read.acquire().await.unwrap();

        // write index to buffer
        buffer[ID_SIZE..INDEX_SIZE + ID_SIZE].copy_from_slice(&index.to_be_bytes());

        // read data into buffer after id and index
        let read = reader.read(&mut buffer[INDEX_SIZE + ID_SIZE..]).await?;

        // check if EOF
        if read == 0 {
            break;
        }

        cipher.apply_keystream(&mut buffer[INDEX_SIZE + ID_SIZE..INDEX_SIZE + ID_SIZE + read]);

        // push job to queue
        queue
            .send(Job {
                data: buffer,
                index,
                id,
                cached_at: None,
            })
            .await?;

        index += read as u64; // increment index by bytes read
        permit.forget(); // release permit
    }

    Ok(())
}
