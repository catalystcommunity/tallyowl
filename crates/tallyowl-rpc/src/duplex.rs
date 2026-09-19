//! A TLS connection that reads and writes at the same time.
//!
//! # Why this exists
//!
//! The plain path serves correlated requests concurrently because it opens a
//! second handle to the same socket: one carrier reads while another writes.
//! `rustls::StreamOwned` cannot be used that way, because it bundles the
//! connection with the socket and does the socket input and output inside a
//! `&mut self` borrow. L058 recorded that limitation as a property of rustls.
//! **It is not.** rustls gives a host `read_tls`, `process_new_packets`,
//! `reader`, `writer`, and `write_tls`, which is everything a duplex carrier
//! needs.
//!
//! [`TlsDuplex`] is that carrier. It holds the rustls connection behind a mutex
//! and the two socket directions behind two more, and it clones into as many
//! handles as a host wants. Two handles over one TLS session read and write at
//! the same time, so the framing above does not know TLS is there and neither
//! the server loop nor the pipelining client changes shape.
//!
//! # The three rules that make it sound
//!
//! **Never hold the connection lock across a blocking socket call.** A read
//! takes raw bytes off the socket first, with no connection lock held, and then
//! takes the lock only to feed them in. A writer builds its records under the
//! lock, releases it, and only then writes them. A reader that blocked while
//! holding the lock would stop every writer, which is the deadlock L058
//! described.
//!
//! **One gate owns record order.** TLS numbers its records, so two threads that
//! produced records and then raced to the socket would deliver them out of
//! order and the peer would close the session. The write gate is held across
//! both steps — take the records out of the connection, then put them on the
//! socket — so records reach the socket in the order the connection made them.
//!
//! **A socket read can carry more than the connection will take.** rustls holds
//! decoded plaintext until the caller reads it, and refuses more input while
//! that store is full. Raw bytes it did not take are kept here and offered again
//! after the caller has drained some plaintext. Feeding in a loop instead is
//! what a first version of this module did, and rustls answered `received
//! plaintext buffer full`, which closed the session under a pipelining client.
//!
//! TLS keeps separate keys and sequence numbers for each direction, so a read
//! and a write at the same time are independent. That is what makes the split
//! sound rather than a trick.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use rustls::Connection;

/// How many raw bytes one socket read takes. A TLS record is at most 16 KiB of
/// plaintext plus its overhead, so this holds a whole record most of the time
/// and never allocates for each read.
const RAW_READ_BYTES: usize = 32 * 1024;

/// The socket's read direction, and whatever it carried that rustls has not
/// taken yet.
struct Input {
    socket: TcpStream,
    /// Raw bytes read from the socket and not yet given to the connection.
    /// Almost always empty; it fills only when the caller is behind.
    waiting: Vec<u8>,
}

/// The state one TLS session shares between its handles.
struct Session {
    /// The rustls connection. Held briefly and never across socket input or
    /// output.
    connection: Mutex<Connection>,
    /// The socket, for reading, and the raw bytes not yet taken.
    input: Mutex<Input>,
    /// The socket, for writing. Held across "take the records out" and "put
    /// them on the socket", so records keep their order.
    output: Mutex<TcpStream>,
}

fn poisoned(what: &str) -> io::Error {
    io::Error::other(format!(
        "The secure connection can no longer be used, because a thread failed while holding its {what}."
    ))
}

fn tls_error(e: rustls::Error) -> io::Error {
    io::Error::other(format!("The secure connection failed: {e}"))
}

/// What one turn of the input pump achieved.
enum Pumped {
    /// Something moved. Try to read plaintext again.
    Progress,
    /// The peer closed the connection.
    Closed,
}

/// One handle on a TLS session. `Read` and `Write` on two handles run at the
/// same time.
///
/// Cloning a handle does not copy the session. Every clone speaks for the same
/// connection and the same socket.
#[derive(Clone)]
pub struct TlsDuplex {
    session: Arc<Session>,
}

impl TlsDuplex {
    /// Wrap a connection and its socket.
    ///
    /// The handshake is not run here. It runs on the first read or write, or
    /// when a host calls [`TlsDuplex::handshake`].
    pub fn new(connection: Connection, socket: TcpStream) -> io::Result<TlsDuplex> {
        let output = socket.try_clone()?;
        Ok(TlsDuplex {
            session: Arc::new(Session {
                connection: Mutex::new(connection),
                input: Mutex::new(Input {
                    socket,
                    waiting: Vec::new(),
                }),
                output: Mutex::new(output),
            }),
        })
    }

    /// Run the handshake to the end.
    ///
    /// A host calls this when it wants a peer with no enrolled identity refused
    /// before a frame is read, rather than at the first read.
    pub fn handshake(&self) -> io::Result<()> {
        loop {
            let handshaking = {
                let connection = self
                    .session
                    .connection
                    .lock()
                    .map_err(|_| poisoned("state"))?;
                connection.is_handshaking()
            };
            if !handshaking {
                return Ok(());
            }
            self.flush_records()?;
            if matches!(self.pump_input()?, Pumped::Closed) {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "The peer closed the connection during the handshake.",
                ));
            }
        }
    }

    /// The peer's certificate chain, leaf first, once the handshake has run.
    pub fn peer_certificates(&self) -> io::Result<Option<Vec<Vec<u8>>>> {
        let connection = self
            .session
            .connection
            .lock()
            .map_err(|_| poisoned("state"))?;
        Ok(connection
            .peer_certificates()
            .map(|chain| chain.iter().map(|one| one.to_vec()).collect()))
    }

    /// Take whatever records the connection has made and put them on the socket.
    ///
    /// The gate is held across both steps. See the module note: two threads that
    /// each produced records and then raced would deliver them out of order.
    fn flush_records(&self) -> io::Result<()> {
        let mut socket = self.session.output.lock().map_err(|_| poisoned("socket"))?;
        loop {
            let mut records = Vec::new();
            {
                let mut connection = self
                    .session
                    .connection
                    .lock()
                    .map_err(|_| poisoned("state"))?;
                while connection.wants_write() {
                    connection.write_tls(&mut records)?;
                }
            }
            if records.is_empty() {
                return Ok(());
            }
            socket.write_all(&records)?;
        }
    }

    /// Move the input side forward by one step.
    ///
    /// One step is either "give the connection some of the raw bytes it has not
    /// taken" or "read more raw bytes off the socket". It is never both, and it
    /// never gives the connection more than it will take, so the caller gets a
    /// chance to drain plaintext between turns.
    fn pump_input(&self) -> io::Result<Pumped> {
        let mut input = self.session.input.lock().map_err(|_| poisoned("socket"))?;

        if input.waiting.is_empty() {
            let mut raw = vec![0u8; RAW_READ_BYTES];
            // The only blocking call here, and the connection lock is not held.
            let read = input.socket.read(&mut raw)?;
            if read == 0 {
                return Ok(Pumped::Closed);
            }
            raw.truncate(read);
            input.waiting = raw;
        }

        let taken = {
            let mut connection = self
                .session
                .connection
                .lock()
                .map_err(|_| poisoned("state"))?;
            // The connection is holding plaintext the caller has not read. More
            // input would be refused, so leave the raw bytes where they are and
            // let the caller drain first.
            if !connection.wants_read() {
                return Ok(Pumped::Progress);
            }
            let mut rest = &input.waiting[..];
            let taken = connection.read_tls(&mut rest)?;
            connection.process_new_packets().map_err(tls_error)?;
            taken
        };
        input.waiting.drain(..taken);
        drop(input);

        // A handshake message or a key update needs an answer, and the peer
        // waits for it. Sending it here is what keeps a connection alive that is
        // only ever read.
        self.flush_records()?;
        Ok(Pumped::Progress)
    }

    /// Read plaintext the connection already holds, without touching the socket.
    fn read_plaintext(&self, buffer: &mut [u8]) -> io::Result<Option<usize>> {
        let mut connection = self
            .session
            .connection
            .lock()
            .map_err(|_| poisoned("state"))?;
        match connection.reader().read(buffer) {
            // Zero means the peer said goodbye properly.
            Ok(read) => Ok(Some(read)),
            // No plaintext yet.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl Read for TlsDuplex {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if let Some(read) = self.read_plaintext(buffer)? {
                return Ok(read);
            }
            if matches!(self.pump_input()?, Pumped::Closed) {
                // The socket ended. Anything the connection still holds comes
                // out here; an empty one reads as end of stream.
                return Ok(self.read_plaintext(buffer)?.unwrap_or(0));
            }
        }
    }
}

impl Write for TlsDuplex {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = {
            let mut connection = self
                .session
                .connection
                .lock()
                .map_err(|_| poisoned("state"))?;
            connection.writer().write(buffer)?
        };
        self.flush_records()?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        {
            let mut connection = self
                .session
                .connection
                .lock()
                .map_err(|_| poisoned("state"))?;
            connection.writer().flush()?;
        }
        self.flush_records()?;
        self.session
            .output
            .lock()
            .map_err(|_| poisoned("socket"))?
            .flush()
    }
}
