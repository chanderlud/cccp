use aes::{Aes128, Aes192, Aes256};
use chacha20::{ChaCha20, ChaCha8};
use cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use ctr::Ctr128BE;
use prost::Message;
use rand::rngs::{OsRng, StdRng};
use rand::{RngCore, SeedableRng};
use tokio::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::items::{Cipher, Crypto};
use crate::Result;

pub(crate) trait StreamCipherWrapper: Send + Sync {
    fn seek(&mut self, index: u64);
    fn apply_keystream(&mut self, data: &mut [u8]);
}

impl<T> StreamCipherWrapper for T
where
    T: StreamCipherSeek + StreamCipher + Send + Sync,
{
    #[inline(always)]
    fn seek(&mut self, index: u64) {
        StreamCipherSeek::seek(self, index);
    }

    #[inline(always)]
    fn apply_keystream(&mut self, buf: &mut [u8]) {
        StreamCipher::apply_keystream(self, buf);
    }
}

pub(crate) struct CipherStream<S: AsyncWrite + AsyncRead + Unpin> {
    stream: S,
    cipher: Box<dyn StreamCipherWrapper>,
}

impl<S: AsyncWrite + AsyncRead + Unpin> CipherStream<S> {
    pub(crate) fn new(stream: S, crypto: &Crypto) -> Result<Self> {
        Ok(Self {
            stream,
            cipher: crypto.make_cipher()?,
        })
    }

    /// write a `Message` to the stream
    pub(crate) async fn write_message<M: Message>(&mut self, message: &M) -> Result<()> {
        let len = message.encoded_len(); // get the length of the message
        self.write_u32(len as u32).await?; // write the length of the message

        let mut buffer = Vec::with_capacity(len); // create a buffer to write the message into
        message.encode(&mut buffer).unwrap(); // encode the message into the buffer (infallible)

        self.write_all(&mut buffer).await?; // write the message to the writer

        Ok(())
    }

    /// read a `Message` from the stream
    pub(crate) async fn read_message<M: Message + Default>(&mut self) -> Result<M> {
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

struct NoCipher;

impl StreamCipherWrapper for NoCipher {
    #[inline(always)]
    fn seek(&mut self, _index: u64) {}
    #[inline(always)]
    fn apply_keystream(&mut self, _data: &mut [u8]) {}
}

impl Crypto {
    /// deterministically derive a new iv from the given iv
    pub(crate) fn next_iv(&mut self) {
        if self.cipher == i32::from(Cipher::None) {
            return;
        }

        // create a seed from the first 8 bytes of the iv
        let seed = u64::from_be_bytes(self.iv[..8].try_into().unwrap());
        // create a random number generator from the seed
        let mut rng = StdRng::seed_from_u64(seed);
        let mut bytes = vec![0; self.iv.len()]; // buffer for new iv
        rng.fill_bytes(&mut bytes); // fill the buffer with random bytes
        self.iv = bytes; // set the new iv
    }

    /// randomize the iv
    pub(crate) fn random_iv(&mut self) {
        if self.cipher == i32::from(Cipher::None) {
            return;
        }

        OsRng.fill_bytes(&mut self.iv);
    }

    /// create a new cipher
    pub(crate) fn make_cipher(&self) -> Result<Box<dyn StreamCipherWrapper>> {
        let cipher: Cipher = self.cipher.try_into()?;
        let key = &self.key[..cipher.key_length()];
        let iv = &self.iv[..cipher.iv_length()];

        Ok(match cipher {
            Cipher::None => Box::new(NoCipher),
            Cipher::Aes128 => Box::new(Ctr128BE::<Aes128>::new(key.into(), iv.into())),
            Cipher::Aes192 => Box::new(Ctr128BE::<Aes192>::new(key.into(), iv.into())),
            Cipher::Aes256 => Box::new(Ctr128BE::<Aes256>::new(key.into(), iv.into())),
            Cipher::Chacha8 => Box::new(ChaCha8::new(key.into(), iv.into())),
            Cipher::Chacha20 => Box::new(ChaCha20::new(key.into(), iv.into())),
        })
    }
}

pub(crate) fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    OsRng.fill_bytes(&mut bytes);
    bytes
}
