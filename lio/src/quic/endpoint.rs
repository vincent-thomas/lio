//! QUIC endpoint implementation.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use quinn_proto::{ConnectionHandle, DatagramEvent};

use super::error::Error;
use super::udp::UdpSocket;
use super::{ClientConfig, ServerConfig};

const MAX_DATAGRAM_SIZE: usize = 65535;

/// Shared endpoint state (excluding socket for async access).
pub(crate) struct EndpointState {
    pub(crate) endpoint: quinn_proto::Endpoint,
    pub(crate) connections: HashMap<ConnectionHandle, quinn_proto::Connection>,
    pub(crate) incoming: Vec<quinn_proto::Incoming>,
}

/// Shared endpoint inner with socket stored separately for async access.
pub(crate) struct EndpointInner {
    pub(crate) state: RefCell<EndpointState>,
    pub(crate) socket: UdpSocket,
    pub(crate) local_addr: SocketAddr,
}

/// A QUIC endpoint that can create client or server connections.
///
/// An endpoint manages the UDP socket and coordinates all QUIC connections.
pub struct Endpoint {
    pub(crate) inner: Rc<EndpointInner>,
}

impl Endpoint {
    /// Creates a client-only endpoint bound to the specified address.
    pub async fn client(bind_addr: SocketAddr) -> Result<Self, Error> {
        let socket = UdpSocket::bind(bind_addr).await?;
        let local_addr = socket.local_addr()?;
        let config = quinn_proto::EndpointConfig::default();
        let endpoint = quinn_proto::Endpoint::new(
            config.into(),
            None,
            !cfg!(target_os = "windows"),
            None,
        );
        Ok(Self {
            inner: Rc::new(EndpointInner {
                state: RefCell::new(EndpointState {
                    endpoint,
                    connections: HashMap::new(),
                    incoming: Vec::new(),
                }),
                socket,
                local_addr,
            }),
        })
    }

    /// Creates a server endpoint bound to the specified address.
    pub async fn server(bind_addr: SocketAddr, config: ServerConfig) -> Result<Self, Error> {
        let socket = UdpSocket::bind(bind_addr).await?;
        let local_addr = socket.local_addr()?;
        let endpoint_config = quinn_proto::EndpointConfig::default();
        let endpoint = quinn_proto::Endpoint::new(
            endpoint_config.into(),
            Some(config.inner.into()),
            !cfg!(target_os = "windows"),
            None,
        );
        Ok(Self {
            inner: Rc::new(EndpointInner {
                state: RefCell::new(EndpointState {
                    endpoint,
                    connections: HashMap::new(),
                    incoming: Vec::new(),
                }),
                socket,
                local_addr,
            }),
        })
    }

    /// Returns the local address this endpoint is bound to.
    pub fn local_addr(&self) -> Result<SocketAddr, Error> {
        Ok(self.inner.local_addr)
    }

    /// Connects to a remote QUIC server.
    pub async fn connect(
        &self,
        addr: SocketAddr,
        server_name: &str,
        config: ClientConfig,
    ) -> Result<Connection, Error> {
        let handle = {
            let mut state = self.inner.state.borrow_mut();
            let (handle, conn) = state
                .endpoint
                .connect(Instant::now(), config.inner, addr, server_name)?;
            state.connections.insert(handle, conn);
            handle
        };

        // Drive the connection until handshake completes
        self.drive_connection(handle).await?;

        // Check if connection handshake completed
        {
            let state = self.inner.state.borrow();
            let conn = state.connections.get(&handle).ok_or(Error::ConnectionRefused)?;
            if conn.is_handshaking() {
                return Err(Error::Connect("handshake incomplete".into()));
            }
        }

        Ok(Connection {
            endpoint: self.inner.clone(),
            handle,
        })
    }

    /// Accepts an incoming connection.
    pub async fn accept(&self) -> Result<Option<Connection>, Error> {
        let deadline = Instant::now() + Duration::from_secs(60);

        loop {
            // Try to receive without blocking for too long
            match self.try_recv_one().await {
                Ok(true) => {
                    // Check for new incoming connections
                    let incoming = self.inner.state.borrow_mut().incoming.pop();
                    if let Some(incoming) = incoming {
                        let handle = {
                            let mut state = self.inner.state.borrow_mut();
                            let mut send_buf = Vec::with_capacity(MAX_DATAGRAM_SIZE);
                            let now = Instant::now();
                            let (handle, conn) = state.endpoint.accept(
                                incoming,
                                now,
                                &mut send_buf,
                                None,
                            ).map_err(|e| Error::Connect(format!("{:?}", e)))?;
                            state.connections.insert(handle, conn);
                            handle
                        };

                        // Transmit any initial response
                        self.flush_connection(handle).await?;

                        // Drive until established
                        self.drive_connection(handle).await?;

                        return Ok(Some(Connection {
                            endpoint: self.inner.clone(),
                            handle,
                        }));
                    }
                }
                Ok(false) => {}
                Err(e) => return Err(e),
            }

            if Instant::now() > deadline {
                return Ok(None);
            }
        }
    }

    /// Try to receive one datagram, returns true if data was received.
    async fn try_recv_one(&self) -> Result<bool, Error> {
        let buf = vec![0u8; MAX_DATAGRAM_SIZE];
        let (buf, n, src) = self.inner.socket.recv_from(buf).await?;

        if n == 0 {
            return Ok(false);
        }

        let data = BytesMut::from(&buf[..n]);
        self.process_datagram(data, src).await?;
        Ok(true)
    }

    /// Receives and processes datagrams until no more are available.
    async fn recv_datagrams(&self) -> Result<(), Error> {
        // Receive one datagram
        let buf = vec![0u8; MAX_DATAGRAM_SIZE];
        let (buf, n, src) = self.inner.socket.recv_from(buf).await?;

        if n > 0 {
            let data = BytesMut::from(&buf[..n]);
            self.process_datagram(data, src).await?;
        }
        Ok(())
    }

    /// Processes a received datagram.
    async fn process_datagram(&self, data: BytesMut, src: SocketAddr) -> Result<(), Error> {
        let mut send_buf = Vec::with_capacity(MAX_DATAGRAM_SIZE);
        let now = Instant::now();
        let local = self.inner.local_addr;

        let event = {
            let mut state = self.inner.state.borrow_mut();
            state.endpoint.handle(now, src, Some(local.ip()), None, data, &mut send_buf)
        };

        match event {
            Some(DatagramEvent::NewConnection(incoming)) => {
                self.inner.state.borrow_mut().incoming.push(incoming);
            }
            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                let mut state = self.inner.state.borrow_mut();
                if let Some(conn) = state.connections.get_mut(&handle) {
                    conn.handle_event(event);
                }
            }
            Some(DatagramEvent::Response(transmit)) => {
                let data = send_buf[..transmit.size].to_vec();
                self.inner.socket.send_to(data, transmit.destination).await?;
            }
            None => {}
        }
        Ok(())
    }

    /// Flushes pending transmits for a connection.
    async fn flush_connection(&self, handle: ConnectionHandle) -> Result<usize, Error> {
        let mut send_buf = Vec::with_capacity(MAX_DATAGRAM_SIZE);
        let mut count = 0;
        const MAX_PACKETS_PER_FLUSH: usize = 10;

        let now = Instant::now();

        loop {
            if count >= MAX_PACKETS_PER_FLUSH {
                break;
            }

            let transmit = {
                let mut state = self.inner.state.borrow_mut();
                let Some(conn) = state.connections.get_mut(&handle) else {
                    return Ok(count);
                };
                conn.poll_transmit(now, 1, &mut send_buf)
            };

            let Some(transmit) = transmit else {
                return Ok(count);
            };

            let data = send_buf[..transmit.size].to_vec();
            self.inner.socket.send_to(data, transmit.destination).await?;
            count += 1;
        }
        Ok(count)
    }

    /// Drives a connection until handshake completes or fails.
    async fn drive_connection(&self, handle: ConnectionHandle) -> Result<(), Error> {
        let deadline = Instant::now() + Duration::from_secs(30);

        loop {
            // Flush pending transmits FIRST (client needs to send before server can respond)
            self.flush_connection(handle).await?;

            // Then receive datagrams (async)
            self.recv_datagrams().await?;

            {
                let mut state = self.inner.state.borrow_mut();

                let Some(conn) = state.connections.get_mut(&handle) else {
                    return Err(Error::ConnectionClosed("connection dropped".into()));
                };

                // Handle timers
                let now = Instant::now();
                if let Some(timeout) = conn.poll_timeout() {
                    if timeout <= now {
                        conn.handle_timeout(now);
                    }
                }

                // Handle endpoint events
                let events: Vec<_> = std::iter::from_fn(|| conn.poll_endpoint_events()).collect();
                for event in events {
                    if let Some(event) = state.endpoint.handle_event(handle, event) {
                        if let Some(conn) = state.connections.get_mut(&handle) {
                            conn.handle_event(event);
                        }
                    }
                }

                // Check if handshake completed
                if let Some(conn) = state.connections.get(&handle) {
                    if !conn.is_handshaking() {
                        return Ok(());
                    }

                    // Check for errors
                    if conn.is_closed() {
                        return Err(Error::ConnectionClosed("connection closed during handshake".into()));
                    }
                }
            }

            // Check timeout
            if Instant::now() > deadline {
                return Err(Error::Timeout);
            }
        }
    }
}

/// A QUIC connection to a remote peer.
///
/// The connection maintains a reference to its parent endpoint for I/O operations.
pub struct Connection {
    pub(crate) endpoint: Rc<EndpointInner>,
    pub(crate) handle: ConnectionHandle,
}

impl Connection {
    /// Returns the remote address of the peer.
    pub fn remote_address(&self) -> SocketAddr {
        self.endpoint
            .state
            .borrow()
            .connections
            .get(&self.handle)
            .map(|c| c.remote_address())
            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)))
    }

    /// Returns whether the connection handshake is complete.
    pub fn is_established(&self) -> bool {
        self.endpoint
            .state
            .borrow()
            .connections
            .get(&self.handle)
            .map(|c| !c.is_handshaking())
            .unwrap_or(false)
    }

    /// Returns whether the connection is closed.
    pub fn is_closed(&self) -> bool {
        self.endpoint
            .state
            .borrow()
            .connections
            .get(&self.handle)
            .map(|c| c.is_closed())
            .unwrap_or(true)
    }
}
