use std::path::{Path, PathBuf};

use tokio::fs::{File, OpenOptions};
use tokio::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) struct MetaData {
    pub(crate) indexes: Vec<u64>, // the confirmed indexes
    writer: File,                 // the metadata file
    file_path: PathBuf,           // the path to the metadata file
}

impl MetaData {
    pub(crate) async fn new(file_path: &Path) -> io::Result<Self> {
        let file_path = Self::format_path(file_path);

        if file_path.exists() {
            // if loading existing metadata fails, create new metadata
            if let Ok(s) = Self::load(&file_path).await {
                return Ok(s);
            }
        }

        let writer = File::create(&file_path).await?; // create metadata file

        Ok(Self {
            indexes: Vec::new(),
            writer,
            file_path,
        })
    }

    // load metadata from file
    pub(crate) async fn load(file_path: &Path) -> io::Result<Self> {
        let file_path = Self::format_path(file_path);

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&file_path)
            .await?; // open file

        let mut buffer = Vec::new(); // create buffer for data
        file.read_to_end(&mut buffer).await?; // read file in buffer

        let indexes = buffer
            .chunks(8)
            .filter_map(|chunk| Some(u64::from_be_bytes(chunk.try_into().ok()?))) // convert to u64
            .collect();

        Ok(Self {
            indexes,
            writer: file,
            file_path,
        })
    }

    // complete an index
    pub(crate) async fn complete(&mut self, index: u64) -> io::Result<()> {
        self.indexes.push(index); // add index to confirmed indexes
        self.writer.write_all(&index.to_be_bytes()).await // write index to file
    }

    // remove metadata file
    pub(crate) async fn remove(&self) -> io::Result<()> {
        tokio::fs::remove_file(&self.file_path).await
    }

    fn format_path(path: &Path) -> PathBuf {
        let mut path = path.to_path_buf();
        path.set_extension("metadata");
        path
    }
}
