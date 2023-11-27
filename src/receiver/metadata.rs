use std::path::PathBuf;

use tokio::fs::File;
use tokio::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};


pub(crate) struct MetaData {
    pub(crate) indexes: Vec<u64>, // the confirmed indexes
    writer: File,                 // the metadata file
}

impl MetaData {
    pub(crate) async fn new(file_path: &PathBuf) -> io::Result<Self> {
        if file_path.exists() {
            // if loading existing metadata fails, create new metadata
            if let Ok(s) = Self::load(file_path).await {
                return Ok(s);
            }
        }

        let writer = File::create(file_path).await?; // create metadata file

        Ok(Self {
            indexes: Vec::new(),
            writer,
        })
    }

    // load metadata from file
    pub(crate) async fn load(file_path: &PathBuf) -> io::Result<Self> {
        let mut file = File::open(&file_path).await?; // open file
        let mut buffer = Vec::new(); // create buffer for data
        file.read_to_end(&mut buffer).await?; // read file in buffer

        let indexes = buffer
            .chunks(8)
            .filter_map(|chunk| Some(u64::from_be_bytes(chunk.try_into().ok()?))) // convert to u64
            .collect();

        Ok(Self {
            indexes,
            writer: file,
        })
    }

    // save metadata to file
    // pub(crate) async fn save(&self) -> Result<()> {
    //     let mut file = File::create(&self.file_path).await?; // create file
    //
    //     // convert indexes to bytes
    //     let buffer = self
    //         .indexes
    //         .iter()
    //         .flat_map(|index| index.to_be_bytes())
    //         .collect();
    //
    //     file.write_all(&buffer).await?; // write buffer to file
    //
    //     Ok(())
    // }

    // complete an index
    pub(crate) async fn complete(&mut self, index: u64) -> io::Result<()> {
        self.indexes.push(index);

        self.writer.write_all(&index.to_be_bytes()).await // write index to file
    }

    // // get a list of completed indexes
    // pub(crate) fn completed_indexes(&self) -> Vec<u64> {
    //     self.indexes
    //         .iter()
    //         .filter(|(_, complete)| **complete) // filter out complete sections
    //         .map(|(index, _)| *index) // convert to owned usize
    //         .collect()
    // }
}
