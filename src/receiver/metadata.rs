use std::io::SeekFrom;
use std::path::{Path, PathBuf};

use tokio::fs::{remove_file, File, OpenOptions};
use tokio::io;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub(crate) struct Metadata {
    pub(crate) initial_index: u64, // the initial index
    writer: File,                  // the metadata file
    file_path: PathBuf,            // the path to the metadata file
}

impl Metadata {
    /// initialize the metadata object
    pub(crate) async fn new(file_path: &Path) -> io::Result<Self> {
        let file_path = Self::format_path(file_path);

        if file_path.exists() {
            if let Ok(metadata) = Self::load(&file_path).await {
                return Ok(metadata);
            }
            // if loading existing metadata fails, create new metadata
        }

        let writer = File::create(&file_path).await?; // create metadata file

        Ok(Self {
            initial_index: 0,
            writer,
            file_path,
        })
    }

    /// complete an index
    pub(crate) async fn complete(&mut self, index: u64) -> io::Result<()> {
        self.writer.write_u64(index).await // write index to file
    }

    /// remove metadata file
    pub(crate) async fn remove(&self) -> io::Result<()> {
        remove_file(&self.file_path).await
    }

    /// load metadata from file
    async fn load(file_path: &Path) -> io::Result<Self> {
        // format the path to the metadata file
        let file_path = Self::format_path(file_path);

        // open the file for reading and writing
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&file_path)
            .await?;

        // seek to the end of the file
        let len = file.seek(SeekFrom::End(0)).await?;

        let mut buf = [0; 8]; // create buffer for data

        // if the file is not empty, seek back 8 bytes from the end and read the last index
        if len > 0 {
            file.seek(SeekFrom::End(-8)).await?;
            file.read_exact(&mut buf).await?;
        }

        // return a new instance of the Metadata struct
        Ok(Self {
            initial_index: u64::from_be_bytes(buf),
            writer: file,
            file_path,
        })
    }

    /// formats the path to the metadata file
    #[inline]
    fn format_path(path: &Path) -> PathBuf {
        path.with_extension("metadata")
    }
}
