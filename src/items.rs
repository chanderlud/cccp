use std::collections::HashMap;
use std::fmt::Display;

include!(concat!(env!("OUT_DIR"), "/cccp.items.rs"));

impl Message {
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

    pub(crate) fn failure(id: u32, reason: u32) -> Self {
        Self {
            message: Some(message::Message::Failure(Failure { id, reason })),
        }
    }

    pub(crate) fn done() -> Self {
        Self {
            message: Some(message::Message::Done(Done {})),
        }
    }
}

impl Cipher {
    /// the length of the key in bytes
    pub(crate) fn key_length(&self) -> usize {
        32
    }

    /// the length of the iv in bytes
    pub(crate) fn iv_length(&self) -> usize {
        match self {
            Self::Chacha20 | Self::Chacha8 => 12,
            Self::Aes => 16,
        }
    }
}

impl Display for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cipher = match self {
            Self::Aes => "aes",
            Self::Chacha8 => "chacha8",
            Self::Chacha20 => "chacha20",
        };

        write!(f, "{}", cipher)
    }
}

impl StartIndex {
    pub(crate) fn new(index: u64) -> Self {
        Self { index }
    }
}
