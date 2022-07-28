//! Streaming bodies for Requests and Responses.
//!
//! Bodies are not buffered by default, so applications don't use memory they don't need.
//!
//! As [hyper](https://docs.rs/hyper) there are two pieces to this:
//!
//! - **The [`HttpBody`](HttpBody) trait** describes all possible bodies.
//!   This allows any body type that implements `HttpBody`, allowing
//!   applications to have fine-grained control over their streaming.
//! - **The [`Body`](Body) concrete type**, which is an implementation of
//!   `HttpBody`, and returned by touche as a "receive stream". It is also a decent default
//!   implementation if you don't have very custom needs of your send streams.

use std::{
    error::Error,
    fs::File,
    io::{self, Cursor, Read},
    sync::mpsc::{self, SendError, Sender},
};

use headers::{HeaderMap, HeaderName, HeaderValue};
pub use http_body::*;

mod http_body;

/// The [`HttpBody`] used on receiving server requests.
/// It is also a good default body to return as responses.
#[derive(Default)]
pub struct Body(Option<BodyInner>);

#[derive(Default)]
enum BodyInner {
    #[default]
    Empty,
    Buffered(Vec<u8>),
    Iter(Box<dyn Iterator<Item = Chunk>>),
    Reader(Box<dyn Read>, Option<usize>),
}

pub struct BodyChannel(Sender<Chunk>);

/// The sender half of a channel, used to stream chunks from another thread.
impl BodyChannel {
    /// Send a chunk of bytes to this body.
    pub fn send<T: Into<Vec<u8>>>(&self, data: T) -> Result<(), SendError<Chunk>> {
        self.0.send(data.into().into())
    }

    /// Send a trailer header. Not that trailers will be buffered, so you are not required to send
    /// then only after sending all the chunks.
    pub fn send_trailer<K, V>(
        &self,
        header: K,
        value: V,
    ) -> Result<(), Box<dyn Error + Send + Sync>>
    where
        K: TryInto<HeaderName>,
        V: TryInto<HeaderValue>,
        <K as TryInto<headers::HeaderName>>::Error: Error + Send + Sync + 'static,
        <V as TryInto<headers::HeaderValue>>::Error: Error + Send + Sync + 'static,
    {
        let mut trailers = HeaderMap::new();
        trailers.insert(header.try_into()?, value.try_into()?);
        Ok(self.send_trailers(trailers)?)
    }

    /// Sends trailers to this body. Not that trailers will be buffered, so you are not required to
    /// send then only after sending all the chunks.
    pub fn send_trailers(&self, trailers: HeaderMap) -> Result<(), SendError<Chunk>> {
        self.0.send(Chunk::Trailers(trailers))
    }
}

impl Body {
    /// Create an empty [`Body`] stream.
    pub fn empty() -> Self {
        Body(Some(BodyInner::Empty))
    }

    /// Create a [`Body`] stream with an associated sender half.
    /// Useful when wanting to stream chunks from another thread.
    pub fn channel() -> (BodyChannel, Self) {
        let (tx, rx) = mpsc::channel();
        let body = Body(Some(BodyInner::Iter(Box::new(rx.into_iter()))));
        (BodyChannel(tx), body)
    }

    /// Create a [`Body`] stream from an Iterator of chunks.
    /// Each item emitted will be written as a separated chunk on chunked encoded requests or
    /// responses.
    #[allow(clippy::should_implement_trait)]
    pub fn from_iter<T: Into<Chunk>>(chunks: impl IntoIterator<Item = T> + 'static) -> Self {
        Body(Some(BodyInner::Iter(Box::new(
            chunks.into_iter().map(|chunk| chunk.into()),
        ))))
    }

    /// Create a [`Body`] stream from an [`Read`], with an optional length.
    pub fn from_reader<T: Into<Option<usize>>>(reader: impl Read + 'static, length: T) -> Self {
        Body(Some(BodyInner::Reader(Box::new(reader), length.into())))
    }
}

impl HttpBody for Body {
    type Reader = BodyReader;
    type Chunks = ChunkIterator;

    fn len(&self) -> Option<u64> {
        match &self.0 {
            Some(BodyInner::Empty) => Some(0),
            Some(BodyInner::Buffered(bytes)) => Some(bytes.len() as u64),
            Some(BodyInner::Iter(_)) => None,
            Some(BodyInner::Reader(_, Some(len))) => Some(*len as u64),
            Some(BodyInner::Reader(_, None)) => None,
            None => None,
        }
    }

    fn into_reader(mut self) -> Self::Reader {
        match self.0.take().unwrap() {
            BodyInner::Empty => BodyReader(BodyReaderInner::Buffered(Cursor::new(Vec::new()))),
            BodyInner::Buffered(bytes) => BodyReader(BodyReaderInner::Buffered(Cursor::new(bytes))),
            BodyInner::Iter(chunks) => {
                let mut chunks = chunks.filter_map(|chunk| match chunk {
                    Chunk::Data(data) => Some(data),
                    Chunk::Trailers(_) => None,
                });
                let cursor = chunks.next().map(Cursor::new);
                BodyReader(BodyReaderInner::Iter(Box::new(chunks), cursor))
            }
            BodyInner::Reader(stream, Some(len)) => {
                BodyReader(BodyReaderInner::Reader(Box::new(stream.take(len as u64))))
            }
            BodyInner::Reader(stream, None) => BodyReader(BodyReaderInner::Reader(stream)),
        }
    }

    fn into_bytes(mut self) -> io::Result<Vec<u8>> {
        match self.0.take().unwrap() {
            BodyInner::Empty => Ok(Vec::new()),
            BodyInner::Buffered(bytes) => Ok(bytes),
            BodyInner::Iter(chunks) => Ok(chunks
                .filter_map(|chunk| match chunk {
                    Chunk::Data(data) => Some(data),
                    Chunk::Trailers(_) => None,
                })
                .flatten()
                .collect()),
            BodyInner::Reader(stream, Some(len)) => {
                let mut buf = Vec::with_capacity(len);
                stream.take(len as u64).read_to_end(&mut buf)?;
                Ok(buf)
            }
            BodyInner::Reader(mut stream, None) => {
                let mut buf = Vec::with_capacity(8 * 1024);
                stream.read_to_end(&mut buf)?;
                Ok(buf)
            }
        }
    }

    fn into_chunks(mut self) -> Self::Chunks {
        match self.0.take().unwrap() {
            BodyInner::Empty => ChunkIterator(None),
            BodyInner::Buffered(bytes) => ChunkIterator(Some(ChunkIteratorInner::Single(bytes))),
            BodyInner::Iter(chunks) => ChunkIterator(Some(ChunkIteratorInner::Iter(chunks))),
            BodyInner::Reader(reader, len) => {
                ChunkIterator(Some(ChunkIteratorInner::Reader(reader, len)))
            }
        }
    }
}

impl Drop for Body {
    fn drop(&mut self) {
        #[allow(unused_must_use)]
        match self.0.take() {
            Some(BodyInner::Reader(ref mut stream, Some(len))) => {
                let mut buf = vec![0_u8; len as usize];
                stream.read_exact(&mut buf);
            }
            Some(BodyInner::Reader(ref mut stream, None)) => {
                let mut buf = Vec::new();
                stream.read_to_end(&mut buf);
            }
            _ => {}
        }
    }
}

impl From<Vec<u8>> for Body {
    fn from(body: Vec<u8>) -> Self {
        Body(Some(BodyInner::Buffered(body)))
    }
}

impl From<&[u8]> for Body {
    fn from(body: &[u8]) -> Self {
        body.to_vec().into()
    }
}

impl From<&str> for Body {
    fn from(body: &str) -> Self {
        body.as_bytes().to_vec().into()
    }
}

impl From<String> for Body {
    fn from(body: String) -> Self {
        body.into_bytes().into()
    }
}

impl TryFrom<File> for Body {
    type Error = io::Error;

    fn try_from(file: File) -> Result<Self, Self::Error> {
        match file.metadata() {
            Ok(meta) if meta.is_file() => Ok(Body::from_reader(file, meta.len() as usize)),
            Ok(_) => Err(io::Error::new(io::ErrorKind::Other, "not a file")),
            Err(err) => Err(err),
        }
    }
}

pub struct BodyReader(BodyReaderInner);

impl BodyReader {
    #[allow(clippy::should_implement_trait)]
    pub fn from_iter(iter: impl IntoIterator<Item = Vec<u8>> + 'static) -> Self {
        let mut iter = iter.into_iter();
        let cursor = iter.next().map(Cursor::new);
        BodyReader(BodyReaderInner::Iter(Box::new(iter), cursor))
    }
}

enum BodyReaderInner {
    Buffered(Cursor<Vec<u8>>),
    Iter(Box<dyn Iterator<Item = Vec<u8>>>, Option<Cursor<Vec<u8>>>),
    Reader(Box<dyn Read>),
}

impl Read for BodyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0 {
            BodyReaderInner::Buffered(ref mut cursor) => cursor.read(buf),
            BodyReaderInner::Reader(ref mut reader) => reader.read(buf),

            // TODO: support for non partial reads here
            BodyReaderInner::Iter(ref mut iter, ref mut leftover) => {
                while let Some(ref mut cursor) = leftover {
                    let read = cursor.read(buf)?;
                    if read > 0 {
                        return Ok(read);
                    }
                    *leftover = iter.next().map(Cursor::new);
                }
                Ok(0)
            }
        }
    }
}

pub struct ChunkIterator(Option<ChunkIteratorInner>);

impl ChunkIterator {
    pub fn from_reader<T: Into<Option<usize>>>(reader: impl Read + 'static, length: T) -> Self {
        Self(Some(ChunkIteratorInner::Reader(
            Box::new(reader),
            length.into(),
        )))
    }
}

enum ChunkIteratorInner {
    Single(Vec<u8>),
    Iter(Box<dyn Iterator<Item = Chunk>>),
    Reader(Box<dyn Read>, Option<usize>),
}

impl Iterator for ChunkIterator {
    type Item = Chunk;

    fn next(&mut self) -> Option<Self::Item> {
        match self.0.take()? {
            ChunkIteratorInner::Single(bytes) => Some(bytes.into()),
            ChunkIteratorInner::Iter(mut iter) => {
                let item = iter.next()?;
                self.0 = Some(ChunkIteratorInner::Iter(iter));
                Some(item)
            }
            ChunkIteratorInner::Reader(mut reader, Some(len)) => {
                let mut buf = vec![0_u8; len];
                reader.read_exact(&mut buf).ok()?;
                Some(buf.into())
            }
            ChunkIteratorInner::Reader(mut reader, None) => {
                let mut buf = [0_u8; 8 * 1024];
                match reader.read(&mut buf).ok()? {
                    0 => None,
                    bytes => {
                        self.0 = Some(ChunkIteratorInner::Reader(reader, None));
                        Some(buf[0..bytes].to_vec().into())
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use crate::{body::HttpBody, Body};

    #[test]
    fn test_body_reader_buffered() {
        let body = Body::from(vec![1_u8, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let mut reader = body.into_reader();

        let mut buf = [0_u8; 4];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);

        let mut buf = [0_u8; 1];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [5]);

        let mut buf = [0_u8; 5];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [6, 7, 8, 9, 10]);
    }

    #[test]
    fn test_body_reader_chunked() {
        let body = Body::from_iter([vec![1, 2, 3], vec![4, 5, 6], vec![7], vec![8, 9], vec![10]]);
        let mut reader = body.into_reader();

        let mut buf = [0_u8; 4];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);

        let mut buf = [0_u8; 1];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [5]);

        let mut buf = [0_u8; 5];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [6, 7, 8, 9, 10]);
    }

    #[test]
    fn test_body_reader_with_unknown_size() {
        let reader = Cursor::new(vec![1_u8, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let body = Body::from_reader(reader, None);
        let mut reader = body.into_reader();

        let mut buf = [0_u8; 4];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);

        let mut buf = [0_u8; 1];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [5]);

        let mut buf = [0_u8; 5];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [6, 7, 8, 9, 10]);
    }

    #[test]
    fn test_body_reader_with_known_size() {
        let reader = Cursor::new(vec![1_u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        let body = Body::from_reader(reader, 10);
        let mut reader = body.into_reader();

        let mut buf = [0_u8; 4];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);

        let mut buf = [0_u8; 1];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [5]);

        let mut buf = [0_u8; 5];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [6, 7, 8, 9, 10]);

        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).unwrap();
        assert!(buf.is_empty());
    }
}
