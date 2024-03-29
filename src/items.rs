use crate::TransferStats;
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::Ordering::Relaxed;

include!(concat!(env!("OUT_DIR"), "/cccp.items.rs"));

impl Message {
    pub(crate) fn manifest(manifest: &Manifest) -> Self {
        Self {
            message: Some(message::Message::Manifest(manifest.clone())),
        }
    }

    pub(crate) fn start(id: u32) -> Self {
        Self {
            message: Some(message::Message::Start(Start { id })),
        }
    }

    pub(crate) fn confirmations(indexes: HashMap<u32, ConfirmationIndexes>) -> Self {
        Self {
            message: Some(message::Message::Confirmations(Confirmations { indexes })),
        }
    }

    pub(crate) fn completed(ids: Vec<u32>) -> Self {
        Self {
            message: Some(message::Message::Completed(Completed { ids })),
        }
    }

    pub(crate) fn end(id: u32) -> Self {
        Self {
            message: Some(message::Message::End(End { id })),
        }
    }

    pub(crate) fn failure(id: u32, reason: u32, description: Option<String>) -> Self {
        Self {
            message: Some(message::Message::Failure(Failure {
                id,
                reason,
                description,
            })),
        }
    }

    pub(crate) fn done(reason: u32) -> Self {
        Self {
            message: Some(message::Message::Done(Done { reason })),
        }
    }
}

impl Cipher {
    /// the length of the key in bytes
    pub(crate) fn key_length(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Aes128 => 16,
            Self::Aes192 => 24,
            Self::Chacha20 | Self::Chacha8 | Self::Aes256 => 32,
        }
    }

    /// the length of the iv in bytes
    pub(crate) fn iv_length(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Chacha20 | Self::Chacha8 => 12,
            Self::Aes256 | Self::Aes128 | Self::Aes192 => 16,
        }
    }
}

impl Display for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cipher = match self {
            Self::None => "NONE",
            Self::Aes128 => "AES128",
            Self::Aes192 => "AES192",
            Self::Aes256 => "AES256",
            Self::Chacha8 => "CHACHA8",
            Self::Chacha20 => "CHACHA20",
        };

        write!(f, "{}", cipher)
    }
}

impl StartIndex {
    pub(crate) fn new(index: u64) -> Self {
        Self { index }
    }
}

impl Manifest {
    pub(crate) fn is_empty(&self) -> bool {
        self.files.is_empty() && self.directories.is_empty()
    }
}

impl Stats {
    pub(crate) fn from(stats: &TransferStats) -> Self {
        Self {
            confirmed_packets: stats.confirmed_packets.load(Relaxed) as u64,
            sent_packets: stats.sent_packets.load(Relaxed) as u64,
            total_data: stats.total_data.load(Relaxed) as u64,
        }
    }
}
