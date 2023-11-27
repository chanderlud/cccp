use std::collections::HashMap;
use std::path::PathBuf;

use bincode::{deserialize, serialize};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::TRANSFER_BUFFER_SIZE;

type Result<T> = std::result::Result<T, MetadataError>;

#[derive(Debug)]
pub(crate) struct MetadataError {
    _kind: MetadataErrorKind,
}

#[derive(Debug)]
enum MetadataErrorKind {
    Io(std::io::Error),
    Bincode(bincode::Error),
}

impl From<std::io::Error> for MetadataError {
    fn from(err: std::io::Error) -> Self {
        Self {
            _kind: MetadataErrorKind::Io(err),
        }
    }
}

impl From<bincode::Error> for MetadataError {
    fn from(err: bincode::Error) -> Self {
        Self {
            _kind: MetadataErrorKind::Bincode(err),
        }
    }
}

pub(crate) struct MetaData {
    // HashMap<index, completed>
    pub(crate) indexes: HashMap<u64, bool>,
    file_path: PathBuf,
}

impl MetaData {
    pub(crate) async fn new(file_size: u64, file_path: &PathBuf) -> Result<Self> {
        if file_path.exists() {
            // if loading existing metadata fails, create new metadata
            if let Ok(s) = Self::load(file_path).await {
                return Ok(s);
            }
        }

        let mut map = HashMap::new();
        let mut index = 0;

        // create sections map
        while index < file_size {
            map.insert(index, false);
            index += TRANSFER_BUFFER_SIZE as u64;
        }

        // no metadata file is created on the disk until the first index is completed
        Ok(Self {
            indexes: map,
            file_path: file_path.clone(),
        })
    }

    // load metadata from file
    pub(crate) async fn load(file_path: &PathBuf) -> Result<Self> {
        let mut file = File::open(&file_path).await?; // open file
        let mut buffer = Vec::new(); // create buffer for data
        file.read_to_end(&mut buffer).await?; // read file in buffer

        // deserialize buffer into HashMap
        let indexes = deserialize(&buffer)?;

        Ok(Self {
            indexes,
            file_path: file_path.clone(),
        })
    }

    // save metadata to file
    pub(crate) async fn save(&self) -> Result<()> {
        let mut file = File::create(&self.file_path).await?; // create file
        let buffer = serialize(&self.indexes).unwrap(); // serialize HashMap into buffer
        file.write_all(&buffer).await?; // write buffer to file

        Ok(())
    }

    // complete an index
    pub(crate) async fn complete(&mut self, index: u64) -> Result<()> {
        self.indexes.insert(index, true); // set index as complete
        self.save().await // save metadata to file
    }

    // get a list of completed indexes
    pub(crate) fn completed_indexes(&self) -> Vec<u64> {
        self.indexes
            .iter()
            .filter(|(_, complete)| **complete) // filter out complete sections
            .map(|(index, _)| *index) // convert to owned usize
            .collect()
    }
}
