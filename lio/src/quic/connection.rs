//! QUIC connection implementation.

use std::time::Instant;

use quinn_proto::{StreamId, VarInt};

use super::endpoint::Connection;
use super::error::Error;
use super::stream::{BiStream, RecvStream, SendStream};

const MAX_DATAGRAM_SIZE: usize = 65535;

impl Connection {
    /// Opens a new bidirectional stream.
    pub async fn open_bi(&self) -> Result<BiStream, Error> {
        loop {
            {
                let mut state = self.endpoint.state.borrow_mut();
                if let Some(conn) = state.connections.get_mut(&self.handle) {
                    if let Some(id) = conn.streams().open(quinn_proto::Dir::Bi) {
                        return Ok(BiStream {
                            send: SendStream::new(id, self.endpoint.clone(), self.handle),
                            recv: RecvStream::new(id, self.endpoint.clone(), self.handle),
                        });
                    }
                }
            }

            // Wait for stream capacity
            self.drive().await?;
            if self.is_closed() {
                return Err(Error::ConnectionClosed("connection closed".into()));
            }
        }
    }

    /// Opens a new unidirectional send stream.
    pub async fn open_uni(&self) -> Result<SendStream, Error> {
        loop {
            {
                let mut state = self.endpoint.state.borrow_mut();
                if let Some(conn) = state.connections.get_mut(&self.handle) {
                    if let Some(id) = conn.streams().open(quinn_proto::Dir::Uni) {
                        return Ok(SendStream::new(id, self.endpoint.clone(), self.handle));
                    }
                }
            }

            self.drive().await?;
            if self.is_closed() {
                return Err(Error::ConnectionClosed("connection closed".into()));
            }
        }
    }

    /// Accepts an incoming bidirectional stream.
    pub async fn accept_bi(&self) -> Result<BiStream, Error> {
        loop {
            {
                let mut state = self.endpoint.state.borrow_mut();
                if let Some(conn) = state.connections.get_mut(&self.handle) {
                    if let Some(id) = conn.streams().accept(quinn_proto::Dir::Bi) {
                        return Ok(BiStream {
                            send: SendStream::new(id, self.endpoint.clone(), self.handle),
                            recv: RecvStream::new(id, self.endpoint.clone(), self.handle),
                        });
                    }
                }
            }

            self.drive().await?;
            if self.is_closed() {
                return Err(Error::ConnectionClosed("connection closed".into()));
            }
        }
    }

    /// Accepts an incoming unidirectional receive stream.
    pub async fn accept_uni(&self) -> Result<RecvStream, Error> {
        loop {
            {
                let mut state = self.endpoint.state.borrow_mut();
                if let Some(conn) = state.connections.get_mut(&self.handle) {
                    if let Some(id) = conn.streams().accept(quinn_proto::Dir::Uni) {
                        return Ok(RecvStream::new(id, self.endpoint.clone(), self.handle));
                    }
                }
            }

            self.drive().await?;
            if self.is_closed() {
                return Err(Error::ConnectionClosed("connection closed".into()));
            }
        }
    }

    /// Closes the connection with the given error code and reason.
    pub fn close(&self, code: u32, reason: &str) {
        let mut state = self.endpoint.state.borrow_mut();
        if let Some(conn) = state.connections.get_mut(&self.handle) {
            conn.close(
                Instant::now(),
                VarInt::from_u32(code),
                bytes::Bytes::copy_from_slice(reason.as_bytes()),
            );
        }
    }

    /// Sends a datagram (unreliable message) to the peer.
    pub fn send_datagram(&self, data: bytes::Bytes) -> Result<(), Error> {
        let mut state = self.endpoint.state.borrow_mut();
        let conn = state
            .connections
            .get_mut(&self.handle)
            .ok_or(Error::ConnectionClosed("connection closed".into()))?;
        conn.datagrams()
            .send(data, true)
            .map_err(|e| Error::Protocol(e.to_string()))
    }

    /// Receives a datagram from the peer.
    pub fn recv_datagram(&self) -> Option<bytes::Bytes> {
        let mut state = self.endpoint.state.borrow_mut();
        state
            .connections
            .get_mut(&self.handle)
            .and_then(|c| c.datagrams().recv())
    }

    /// Drives the connection, processing I/O and timers.
    pub async fn drive(&self) -> Result<(), Error> {
        // Receive datagrams (async)
        self.recv_datagram_async().await?;

        // Send any pending transmissions and handle events
        self.flush_and_process().await?;

        Ok(())
    }

    async fn recv_datagram_async(&self) -> Result<(), Error> {
        let buf = vec![0u8; MAX_DATAGRAM_SIZE];
        let (buf, n, src) = self.endpoint.socket.recv_from(buf).await?;

        if n > 0 {
            let data = bytes::BytesMut::from(&buf[..n]);
            self.process_datagram(data, src).await?;
        }
        Ok(())
    }

    async fn process_datagram(&self, data: bytes::BytesMut, src: std::net::SocketAddr) -> Result<(), Error> {
        use quinn_proto::DatagramEvent;

        let mut send_buf = Vec::with_capacity(MAX_DATAGRAM_SIZE);
        let now = Instant::now();
        let local = self.endpoint.local_addr;

        let event = {
            let mut state = self.endpoint.state.borrow_mut();
            state.endpoint.handle(now, src, Some(local.ip()), None, data, &mut send_buf)
        };

        match event {
            Some(DatagramEvent::NewConnection(incoming)) => {
                self.endpoint.state.borrow_mut().incoming.push(incoming);
            }
            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                let mut state = self.endpoint.state.borrow_mut();
                if let Some(conn) = state.connections.get_mut(&handle) {
                    conn.handle_event(event);
                }
            }
            Some(DatagramEvent::Response(transmit)) => {
                let data = send_buf[..transmit.size].to_vec();
                self.endpoint.socket.send_to(data, transmit.destination).await?;
            }
            None => {}
        }
        Ok(())
    }

    async fn flush_and_process(&self) -> Result<(), Error> {
        // First, flush all pending transmits
        self.flush_transmits().await?;

        // Handle timers and endpoint events
        let mut state = self.endpoint.state.borrow_mut();
        let Some(conn) = state.connections.get_mut(&self.handle) else {
            return Ok(());
        };

        let now = Instant::now();

        // Handle timers
        if let Some(timeout) = conn.poll_timeout() {
            if timeout <= now {
                conn.handle_timeout(now);
            }
        }

        // Handle endpoint events
        let events: Vec<_> = std::iter::from_fn(|| conn.poll_endpoint_events()).collect();
        for event in events {
            if let Some(event) = state.endpoint.handle_event(self.handle, event) {
                if let Some(conn) = state.connections.get_mut(&self.handle) {
                    conn.handle_event(event);
                }
            }
        }

        Ok(())
    }

    async fn flush_transmits(&self) -> Result<usize, Error> {
        let mut send_buf = Vec::with_capacity(MAX_DATAGRAM_SIZE);
        let mut count = 0;
        const MAX_PACKETS_PER_FLUSH: usize = 10;

        let now = Instant::now();

        loop {
            if count >= MAX_PACKETS_PER_FLUSH {
                break;
            }

            let transmit = {
                let mut state = self.endpoint.state.borrow_mut();
                let Some(conn) = state.connections.get_mut(&self.handle) else {
                    return Ok(count);
                };
                conn.poll_transmit(now, 1, &mut send_buf)
            };

            let Some(transmit) = transmit else {
                return Ok(count);
            };

            let data = send_buf[..transmit.size].to_vec();
            self.endpoint.socket.send_to(data, transmit.destination).await?;
            count += 1;
        }
        Ok(count)
    }

    pub(crate) async fn write_stream(&self, id: StreamId, data: &[u8]) -> Result<usize, Error> {
        let result = {
            let mut state = self.endpoint.state.borrow_mut();
            let conn = state
                .connections
                .get_mut(&self.handle)
                .ok_or(Error::ConnectionClosed("connection closed".into()))?;
            conn.send_stream(id).write(data)?
        };

        // Flush transmit
        self.flush_transmits().await?;

        Ok(result)
    }

    pub(crate) fn read_stream(&self, id: StreamId, buf: &mut [u8]) -> Result<Option<usize>, Error> {
        let mut state = self.endpoint.state.borrow_mut();
        let conn = state
            .connections
            .get_mut(&self.handle)
            .ok_or(Error::ConnectionClosed("connection closed".into()))?;

        let mut recv = conn.recv_stream(id);
        let mut chunks = recv.read(true)?;

        match chunks.next(buf.len()) {
            Ok(Some(chunk)) => {
                let len = chunk.bytes.len().min(buf.len());
                buf[..len].copy_from_slice(&chunk.bytes[..len]);
                let _ = chunks.finalize();
                Ok(Some(len))
            }
            Ok(None) => {
                let _ = chunks.finalize();
                Ok(None)
            }
            // Blocked means no data available yet - treat as Ok(None) to retry
            Err(quinn_proto::ReadError::Blocked) => {
                let _ = chunks.finalize();
                Ok(None)
            }
            Err(quinn_proto::ReadError::Reset(_)) => Ok(None),
        }
    }

    pub(crate) async fn finish_stream(&self, id: StreamId) -> Result<(), Error> {
        {
            let mut state = self.endpoint.state.borrow_mut();
            let conn = state
                .connections
                .get_mut(&self.handle)
                .ok_or(Error::ConnectionClosed("connection closed".into()))?;
            conn.send_stream(id).finish()?;
        }

        // Flush transmit
        self.flush_transmits().await?;

        Ok(())
    }

    pub(crate) fn stop_stream(&self, id: StreamId, code: u32) {
        let mut state = self.endpoint.state.borrow_mut();
        if let Some(conn) = state.connections.get_mut(&self.handle) {
            let _ = conn.recv_stream(id).stop(VarInt::from_u32(code));
        }
    }
}
