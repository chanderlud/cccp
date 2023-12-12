include!(concat!(env!("OUT_DIR"), "/cccp.items.rs"));

impl Message {
    pub(crate) fn start(id: u32) -> Self {
        Self {
            message: Some(message::Message::Start(Start { id })),
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
