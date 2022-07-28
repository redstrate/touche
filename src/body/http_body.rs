use std::{
    io::{self, Cursor, Read},
    iter,
};

use headers::HeaderMap;

/// Trait representing a streaming body
pub trait HttpBody: Sized {
    type Reader: Read;
    type Chunks: Iterator<Item = Chunk>;

    /// The lenght of a body, when it is known.
    fn len(&self) -> Option<u64>;

    /// Returns if this body is empty.
    /// Note that unknown sized bodies (such as close delimited or chunked encoded) will never be
    /// considered to be empty.
    fn is_empty(&self) -> bool {
        matches!(self.len(), Some(0))
    }

    /// Consumes this body and returns a [`Read`].
    fn into_reader(self) -> Self::Reader;

    /// Consumes this body in chunks.
    fn into_chunks(self) -> Self::Chunks;

    /// Consumes this body and returns its bytes.
    fn into_bytes(self) -> io::Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(1024);
        self.into_reader().read_to_end(&mut buf)?;
        Ok(buf)
    }
}

impl HttpBody for () {
    type Reader = io::Empty;
    type Chunks = iter::Empty<Chunk>;

    fn len(&self) -> Option<u64> {
        Some(0)
    }

    fn into_reader(self) -> Self::Reader {
        io::empty()
    }

    fn into_bytes(self) -> io::Result<Vec<u8>> {
        Ok(Vec::new())
    }

    fn into_chunks(self) -> Self::Chunks {
        iter::empty()
    }
}

impl HttpBody for String {
    type Reader = Cursor<Vec<u8>>;
    type Chunks = iter::Once<Chunk>;

    fn len(&self) -> Option<u64> {
        self.len().try_into().ok()
    }

    fn into_reader(self) -> Self::Reader {
        Cursor::new(self.into_bytes())
    }

    fn into_bytes(self) -> io::Result<Vec<u8>> {
        Ok(self.into_bytes())
    }

    fn into_chunks(self) -> Self::Chunks {
        iter::once(self.into_bytes().into())
    }
}

impl HttpBody for &str {
    type Reader = Cursor<Vec<u8>>;
    type Chunks = iter::Once<Chunk>;

    fn len(&self) -> Option<u64> {
        str::len(self).try_into().ok()
    }

    fn into_reader(self) -> Self::Reader {
        Cursor::new(self.bytes().collect())
    }

    fn into_bytes(self) -> io::Result<Vec<u8>> {
        Ok(self.bytes().collect())
    }

    fn into_chunks(self) -> Self::Chunks {
        iter::once(Chunk::Data(self.bytes().collect()))
    }
}

impl HttpBody for &'static [u8] {
    type Reader = &'static [u8];
    type Chunks = iter::Once<Chunk>;

    fn len(&self) -> Option<u64> {
        (*self).len().try_into().ok()
    }

    fn into_reader(self) -> Self::Reader {
        self
    }

    fn into_bytes(self) -> io::Result<Vec<u8>> {
        Ok(self.to_vec())
    }

    fn into_chunks(self) -> Self::Chunks {
        iter::once(self.to_vec().into())
    }
}

impl HttpBody for Vec<u8> {
    type Reader = Cursor<Vec<u8>>;
    type Chunks = iter::Once<Chunk>;

    fn len(&self) -> Option<u64> {
        self.len().try_into().ok()
    }

    fn into_reader(self) -> Self::Reader {
        Cursor::new(self)
    }

    fn into_bytes(self) -> io::Result<Vec<u8>> {
        Ok(self)
    }

    fn into_chunks(self) -> Self::Chunks {
        iter::once(self.into())
    }
}

/// A message of a chunked encoded body.
pub enum Chunk {
    /// Data chunk.
    Data(Vec<u8>),
    /// [Trailers](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Trailer) header chunk.
    Trailers(HeaderMap),
}

impl<T: Into<Vec<u8>>> From<T> for Chunk {
    fn from(chunk: T) -> Self {
        Self::Data(chunk.into())
    }
}
