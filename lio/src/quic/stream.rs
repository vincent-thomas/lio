//! QUIC stream types.

use std::rc::Rc;

use quinn_proto::{ConnectionHandle, StreamId};

use super::endpoint::{Connection, EndpointInner};
use super::error::Error;

/// A bidirectional QUIC stream.
pub struct BiStream {
    /// The send half of the stream.
    pub send: SendStream,
    /// The receive half of the stream.
    pub recv: RecvStream,
}

/// A QUIC send stream.
pub struct SendStream {
    id: StreamId,
    endpoint: Rc<EndpointInner>,
    handle: ConnectionHandle,
}

impl SendStream {
    pub(crate) fn new(id: StreamId, endpoint: Rc<EndpointInner>, handle: ConnectionHandle) -> Self {
        Self {
            id,
            endpoint,
            handle,
        }
    }

    /// Writes data to the stream.
    ///
    /// Returns the number of bytes written.
    pub async fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        let conn = Connection {
            endpoint: self.endpoint.clone(),
            handle: self.handle,
        };

        loop {
            match conn.write_stream(self.id, buf).await {
                Ok(n) if n > 0 => return Ok(n),
                Ok(_) => {
                    // Buffer full, wait for capacity
                    conn.drive().await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Writes all data to the stream.
    pub async fn write_all(&mut self, mut buf: &[u8]) -> Result<(), Error> {
        while !buf.is_empty() {
            let n = self.write(buf).await?;
            buf = &buf[n..];
        }
        Ok(())
    }

    /// Finishes the stream, indicating no more data will be sent.
    pub async fn finish(&mut self) -> Result<(), Error> {
        let conn = Connection {
            endpoint: self.endpoint.clone(),
            handle: self.handle,
        };
        conn.finish_stream(self.id).await
    }

    /// Returns the stream ID.
    pub fn id(&self) -> StreamId {
        self.id
    }
}

/// A QUIC receive stream.
pub struct RecvStream {
    id: StreamId,
    endpoint: Rc<EndpointInner>,
    handle: ConnectionHandle,
}

impl RecvStream {
    pub(crate) fn new(id: StreamId, endpoint: Rc<EndpointInner>, handle: ConnectionHandle) -> Self {
        Self {
            id,
            endpoint,
            handle,
        }
    }

    /// Reads data from the stream.
    ///
    /// Returns `Ok(Some(n))` if data was read, `Ok(None)` if the stream ended,
    /// or an error.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, Error> {
        let conn = Connection {
            endpoint: self.endpoint.clone(),
            handle: self.handle,
        };

        loop {
            match conn.read_stream(self.id, buf) {
                Ok(Some(n)) => return Ok(Some(n)),
                Ok(None) => {
                    // No data available yet, drive and retry
                    conn.drive().await?;

                    // Check if connection closed
                    if conn.is_closed() {
                        return Ok(None);
                    }
                }
                Err(Error::StreamClosed) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    /// Reads data until the buffer is full or the stream ends.
    pub async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        let mut filled = 0;
        while filled < buf.len() {
            match self.read(&mut buf[filled..]).await? {
                Some(n) => filled += n,
                None => return Err(Error::StreamClosed),
            }
        }
        Ok(())
    }

    /// Reads all remaining data from the stream.
    pub async fn read_to_end(&mut self, buf: &mut Vec<u8>) -> Result<usize, Error> {
        let start_len = buf.len();
        let mut chunk = [0u8; 4096];

        loop {
            match self.read(&mut chunk).await? {
                Some(n) => buf.extend_from_slice(&chunk[..n]),
                None => break,
            }
        }

        Ok(buf.len() - start_len)
    }

    /// Stops reading from the stream with the given error code.
    pub fn stop(&mut self, code: u32) {
        let conn = Connection {
            endpoint: self.endpoint.clone(),
            handle: self.handle,
        };
        conn.stop_stream(self.id, code);
    }

    /// Returns the stream ID.
    pub fn id(&self) -> StreamId {
        self.id
    }
}
