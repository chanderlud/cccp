use aes::{Aes128, Aes256};
use chacha20::{ChaCha20, ChaCha8};
use cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use ctr::Ctr128BE;
use prost::Message;
use tokio::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::items::{Cipher, Crypto};

pub(crate) trait StreamCipherWrapper: Send + Sync {
    fn seek(&mut self, index: u64);
    fn apply_keystream(&mut self, data: &mut [u8]);
}

impl<T> StreamCipherWrapper for T
where
    T: StreamCipherSeek + StreamCipher + Send + Sync,
{
    fn seek(&mut self, index: u64) {
        StreamCipherSeek::seek(self, index);
    }

    fn apply_keystream(&mut self, buf: &mut [u8]) {
        StreamCipher::apply_keystream(self, buf);
    }
}

pub(crate) struct CipherStream<S: AsyncWrite + AsyncRead + Unpin> {
    stream: S,
    cipher: Box<dyn StreamCipherWrapper>,
}

impl<S: AsyncWrite + AsyncRead + Unpin> CipherStream<S> {
    pub(crate) fn new(stream: S, crypto: &Crypto) -> crate::Result<Self> {
        Ok(Self {
            stream,
            cipher: make_cipher(crypto)?,
        })
    }

    /// write a `Message` to the stream
    pub(crate) async fn write_message<M: Message>(&mut self, message: &M) -> crate::Result<()> {
        let len = message.encoded_len(); // get the length of the message
        self.write_u32(len as u32).await?; // write the length of the message

        let mut buffer = Vec::with_capacity(len); // create a buffer to write the message into
        message.encode(&mut buffer).unwrap(); // encode the message into the buffer (infallible)

        self.write_all(&mut buffer).await?; // write the message to the writer

        Ok(())
    }

    /// read a `Message` from the stream
    pub(crate) async fn read_message<M: Message + Default>(&mut self) -> crate::Result<M> {
        let len = self.read_u32().await? as usize; // read the length of the message

        let mut buffer = vec![0; len]; // create a buffer to read the message into
        self.read_exact(&mut buffer).await?; // read the message into the buffer

        let message = M::decode(&buffer[..])?; // decode the message

        Ok(message)
    }

    async fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        AsyncReadExt::read_exact(&mut self.stream, buf).await?;
        self.cipher.apply_keystream(buf);
        Ok(())
    }

    async fn read_u32(&mut self) -> io::Result<u32> {
        let mut buf = [0; 4];
        self.read_exact(&mut buf).await?;
        Ok(u32::from_be_bytes(buf))
    }

    async fn write_all(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.cipher.apply_keystream(buf);
        AsyncWriteExt::write_all(&mut self.stream, buf).await
    }

    async fn write_u32(&mut self, value: u32) -> io::Result<()> {
        let mut buf = value.to_be_bytes();
        self.write_all(&mut buf).await
    }
}

pub(crate) fn make_cipher(crypto: &Crypto) -> crate::Result<Box<dyn StreamCipherWrapper>> {
    let cipher: Cipher = crypto.cipher.try_into()?;
    let key = &crypto.key[..cipher.key_length()];
    let iv = &crypto.iv[..cipher.iv_length()];

    Ok(match cipher {
        Cipher::Aes128 => Box::new(Ctr128BE::<Aes128>::new(key.into(), iv.into())),
        Cipher::Aes256 => Box::new(Ctr128BE::<Aes256>::new(key.into(), iv.into())),
        Cipher::Chacha8 => Box::new(ChaCha8::new(key.into(), iv.into())),
        Cipher::Chacha20 => Box::new(ChaCha20::new(key.into(), iv.into())),
    })
}
