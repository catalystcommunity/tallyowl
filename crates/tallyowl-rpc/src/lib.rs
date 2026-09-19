//! CSIL-RPC over TCP, for a TallyOwl service and for a TallyOwl client.
//!
//! The generated code provides types, codecs, and routing seams. It does not
//! provide the request-to-response loop, which belongs to the host. This crate
//! is that host code: an accept loop on threads, a reconnecting client, and the
//! one rule that decides how an application error travels.
//!
//! **An application error is not a transport failure.** A `ServiceError` rides
//! back with transport status 0 and the variant name `ServiceError`. A non-zero
//! transport status means the transport could not deliver a typed reply at all.
//! Conflating the two makes a caller retry a permanent rejection, or give up on
//! a connection reset.
//!
//! Native TallyOwl server-to-server traffic uses CSIL over TCP. There is no
//! generic HTTP ingest API here, and there is not going to be one.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use csilgen_transport::carrier::{FrameCarrier, StreamCarrier};
use csilgen_transport::rpc::{HandlerOutcome, RpcClient, RpcRequest, RpcResponse};
use csilgen_transport::Status;
use tallyowl_obs::error::{ErrorCode, TallyOwlError};

pub use csilgen_transport::rpc::{
    HandlerOutcome as Outcome, RpcRequest as Request, RpcResponse as Response,
};
pub use csilgen_transport::Status as TransportStatus;

/// The variant name an application error travels under. Every TallyOwl service
/// declares `ServiceError` as its error arm, so this is one constant rather than
/// a string at each call site.
pub const SERVICE_ERROR_VARIANT: &str = "ServiceError";

/// How many correlated requests one connection serves at the same time.
///
/// A request that carries a correlation ID may be answered out of order, so a
/// connection is not obliged to finish one batch before it starts the next.
/// `docs/DESIGN.md` section 4.2 calls the app-to-collector hop "pipelined RPC",
/// and this is the server half of that word: without it a pipelining client
/// gains only the network latency, because every batch would still queue behind
/// the durable write of the batch in front of it.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 8;

/// How many correlated requests a pipelining client keeps outstanding.
///
/// Four covers the measured round trip at the D19 batch defaults. It is a
/// window, not a buffer: the driver still refuses at its unacknowledged bound.
pub const DEFAULT_CLIENT_WINDOW: usize = 4;

/// A counting semaphore, so a connection cannot start unbounded work.
///
/// The read loop blocks when every permit is taken, which stops reading, which
/// fills the receive window and pushes back on the sender. Backpressure travels
/// down the socket rather than into memory.
struct Permits {
    free: Mutex<usize>,
    ready: Condvar,
}

impl Permits {
    fn new(count: usize) -> Permits {
        Permits {
            free: Mutex::new(count.max(1)),
            ready: Condvar::new(),
        }
    }

    fn acquire(&self) {
        let mut free = self.free.lock().expect("permit lock");
        while *free == 0 {
            free = self.ready.wait(free).expect("permit wait");
        }
        *free -= 1;
    }

    fn release(&self) {
        *self.free.lock().expect("permit lock") += 1;
        self.ready.notify_one();
    }
}

/// What a service does with one decoded request.
///
/// The implementation is normally a match over `request.op` that decodes, calls
/// a typed handler, and encodes. `docs/DELIVERY.md` decides what each arm does;
/// this trait only decides where it lives.
pub trait Dispatcher: Send + Sync + 'static {
    fn dispatch(&self, request: &RpcRequest) -> HandlerOutcome;
}

impl<F> Dispatcher for F
where
    F: Fn(&RpcRequest) -> HandlerOutcome + Send + Sync + 'static,
{
    fn dispatch(&self, request: &RpcRequest) -> HandlerOutcome {
        self(request)
    }
}

/// Build the outcome for an application error.
pub fn error_outcome(payload: Vec<u8>) -> HandlerOutcome {
    HandlerOutcome::Reply {
        variant: SERVICE_ERROR_VARIANT.to_string(),
        payload,
    }
}

/// Build the outcome for a successful reply.
pub fn reply(variant: &str, payload: Vec<u8>) -> HandlerOutcome {
    HandlerOutcome::Reply {
        variant: variant.to_string(),
        payload,
    }
}

/// The outcome for a request this service does not know. This is a transport
/// failure, not an application error: no handler ran, so no typed reply exists.
pub fn unknown_operation(service: &str, op: &str) -> HandlerOutcome {
    HandlerOutcome::Transport(
        Status::UnknownServiceOrOp,
        format!("This service does not have an operation named `{op}` on `{service}`."),
    )
}

/// The outcome for a request whose payload did not decode.
pub fn malformed(reason: impl std::fmt::Display) -> HandlerOutcome {
    HandlerOutcome::Transport(
        Status::MalformedEnvelope,
        format!("The request could not be read: {reason}"),
    )
}

pub mod duplex;
pub mod tls;

/// What a connection is carried on.
///
/// A plain socket and a TLS session behave the same above the framing, so the
/// client, the pipeline, and the server loop are written once and take either.
/// The enum rather than a boxed trait object keeps the read path free of a
/// virtual call for each frame.
pub enum Wire {
    Plain(TcpStream),
    Secure(duplex::TlsDuplex),
}

impl Wire {
    /// A second handle on the same connection, so one side can read while the
    /// other writes.
    pub fn try_clone(&self) -> std::io::Result<Wire> {
        match self {
            Wire::Plain(stream) => stream.try_clone().map(Wire::Plain),
            Wire::Secure(tls) => Ok(Wire::Secure(tls.clone())),
        }
    }
}

impl Read for Wire {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(stream) => stream.read(buffer),
            Wire::Secure(tls) => tls.read(buffer),
        }
    }
}

impl Write for Wire {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(stream) => stream.write(buffer),
            Wire::Secure(tls) => tls.write(buffer),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Wire::Plain(stream) => stream.flush(),
            Wire::Secure(tls) => tls.flush(),
        }
    }
}

/// A running CSIL-RPC listener.
pub struct Server {
    local_address: std::net::SocketAddr,
    stopping: Arc<AtomicBool>,
}

impl Server {
    pub(crate) fn new(local_address: std::net::SocketAddr, stopping: Arc<AtomicBool>) -> Server {
        Server {
            local_address,
            stopping,
        }
    }

    pub fn local_address(&self) -> std::net::SocketAddr {
        self.local_address
    }

    /// Stop accepting. An open connection finishes the frame it is serving.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        // Wake the accept loop so it observes the flag rather than waiting for
        // the next real connection.
        let _ = TcpStream::connect(self.local_address);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Serve CSIL-RPC on `address`. Binding to port 0 gives an operating-system
/// port, which is what an integration test wants.
///
/// Each accepted connection is handled on its own thread and stays open, so an
/// app driver keeps one persistent, reconnecting connection rather than paying
/// for a handshake for each batch. See D5.
pub fn serve(
    address: &str,
    dispatcher: Arc<dyn Dispatcher>,
    max_frame_bytes: usize,
) -> std::io::Result<Server> {
    serve_with_in_flight(address, dispatcher, max_frame_bytes, DEFAULT_MAX_IN_FLIGHT)
}

/// Serve CSIL-RPC, and say how many correlated requests one connection may
/// serve at the same time.
///
/// A request that carries no correlation ID keeps the strict one-at-a-time
/// path, because a client that did not ask for correlation cannot tell two
/// replies apart. A request that carries one is answered as soon as it is
/// ready, in whatever order that turns out to be.
pub fn serve_with_in_flight(
    address: &str,
    dispatcher: Arc<dyn Dispatcher>,
    max_frame_bytes: usize,
    max_in_flight: usize,
) -> std::io::Result<Server> {
    let listener = TcpListener::bind(address)?;
    let local_address = listener.local_addr()?;
    let stopping = Arc::new(AtomicBool::new(false));
    let loop_stopping = Arc::clone(&stopping);

    std::thread::Builder::new()
        .name("tallyowl-rpc-accept".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if loop_stopping.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let dispatcher = Arc::clone(&dispatcher);
                let connection_stopping = Arc::clone(&loop_stopping);
                std::thread::spawn(move || {
                    let Ok(write_side) = stream.try_clone() else {
                        return;
                    };
                    serve_connection(
                        Wire::Plain(stream),
                        Wire::Plain(write_side),
                        dispatcher,
                        max_frame_bytes,
                        max_in_flight,
                        connection_stopping,
                    );
                });
            }
        })?;

    Ok(Server {
        local_address,
        stopping,
    })
}

/// One connection: read frames, dispatch, write replies.
///
/// Reading and writing use two carriers over two handles to the same
/// connection, so a worker can write a reply while the read loop is still
/// reading the next request. TCP is full duplex, TLS keeps separate keys and
/// sequence numbers for each direction, and the framing is self-delimiting each
/// way, so the two never interleave.
pub(crate) fn serve_connection<S>(
    read_side: S,
    write_side: S,
    dispatcher: Arc<dyn Dispatcher>,
    max_frame_bytes: usize,
    max_in_flight: usize,
    stopping: Arc<AtomicBool>,
) where
    S: Read + Write + Send + 'static,
{
    // A carrier with a host-chosen limit refuses an oversized frame before it
    // allocates for it, so a decompression bomb never reaches memory.
    let Ok(mut reader) = StreamCarrier::with_max_frame(read_side, max_frame_bytes) else {
        return;
    };
    let Ok(writer) = StreamCarrier::with_max_frame(write_side, max_frame_bytes) else {
        return;
    };
    let writer = Arc::new(Mutex::new(writer));
    let permits = Arc::new(Permits::new(max_in_flight));

    while !stopping.load(Ordering::Relaxed) {
        // A clean end of stream, or a carrier that failed. Either way this
        // connection is finished.
        let frame = match reader.recv_frame() {
            Ok(Some(frame)) => frame,
            Ok(None) | Err(_) => break,
        };

        let request = match RpcRequest::decode(&frame) {
            Ok(request) => request,
            Err(e) => {
                // A frame nobody could decode carries no correlation ID, so the
                // reply cannot be matched to it and the stream position is no
                // longer trustworthy. Say so and close.
                let reply = RpcResponse::transport_error(Status::MalformedEnvelope, e.to_string());
                write_reply(&writer, &reply);
                break;
            }
        };

        match request.id {
            // No correlation ID means one reply at a time, in order.
            None => {
                let reply = outcome_to_reply(dispatcher.dispatch(&request), None);
                if !write_reply(&writer, &reply) {
                    break;
                }
            }
            Some(id) => {
                permits.acquire();
                let worker_dispatcher = Arc::clone(&dispatcher);
                let worker_writer = Arc::clone(&writer);
                let worker_permits = Arc::clone(&permits);
                let started = std::thread::Builder::new()
                    .name("tallyowl-rpc-serve".into())
                    .spawn(move || {
                        let reply =
                            outcome_to_reply(worker_dispatcher.dispatch(&request), Some(id));
                        write_reply(&worker_writer, &reply);
                        worker_permits.release();
                    });
                if started.is_err() {
                    // The host would not give us a thread. Say so against this
                    // request's ID rather than leaving the caller waiting for a
                    // reply that is never coming.
                    permits.release();
                    let reply = RpcResponse::transport_error(
                        Status::Unavailable,
                        "This service could not start work for the request. Send it again."
                            .to_string(),
                    )
                    .with_id(Some(id));
                    if !write_reply(&writer, &reply) {
                        break;
                    }
                }
            }
        }
    }
}

pub(crate) fn outcome_to_reply(outcome: HandlerOutcome, id: Option<u64>) -> RpcResponse {
    match outcome {
        HandlerOutcome::Reply { variant, payload } => RpcResponse::ok(variant, payload).with_id(id),
        HandlerOutcome::Transport(status, message) => {
            RpcResponse::transport_error(status, message).with_id(id)
        }
    }
}

/// Write one reply. Returns false when the connection can no longer carry one.
fn write_reply<S: Read + Write>(writer: &Mutex<StreamCarrier<S>>, reply: &RpcResponse) -> bool {
    let Ok(encoded) = reply.encode() else {
        return false;
    };
    let mut writer = match writer.lock() {
        Ok(writer) => writer,
        // A worker panicked mid-write, so the stream position is unknown.
        Err(_) => return false,
    };
    writer.send_frame(&encoded).is_ok()
}

/// A persistent, reconnecting client for one address.
///
/// The connection is the unit of reconnection, not the call. A call that fails
/// on a broken connection is retried once on a fresh one, because a peer that
/// restarted is the ordinary case and a caller should not have to write that
/// loop. A call that fails twice returns `unavailable`, which is retryable, and
/// the caller decides what to do next.
pub struct Client {
    address: String,
    connect_timeout: Duration,
    max_frame_bytes: usize,
    /// The credential every call on this connection carries.
    ///
    /// The connection is where a credential belongs, not the call: one
    /// application holds one key, and a service that read a key from each
    /// request body would let one connection speak for two tenants.
    credential: Option<String>,
    /// The identity to present, when this connection is secured. `None` is the
    /// plain path, which is what the home profile uses.
    secure: Option<tls::Secure>,
    inner: Mutex<Option<RpcClient<StreamCarrier<Wire>>>>,
}

impl Client {
    pub fn new(address: impl Into<String>, max_frame_bytes: usize) -> Client {
        Client {
            address: address.into(),
            connect_timeout: Duration::from_secs(5),
            max_frame_bytes,
            credential: None,
            secure: None,
            inner: Mutex::new(None),
        }
    }

    /// A client that presents `identity` and verifies the peer against the
    /// installation authority.
    pub fn secure(
        address: impl Into<String>,
        max_frame_bytes: usize,
        identity: tls::Identity,
    ) -> Result<Client, TallyOwlError> {
        Ok(Client {
            secure: Some(tls::Secure::new(identity)?),
            ..Client::new(address, max_frame_bytes)
        })
    }

    /// Present this credential on every call.
    pub fn with_credential(mut self, credential: impl Into<String>) -> Client {
        let credential = credential.into();
        self.credential = (!credential.is_empty()).then_some(credential);
        self
    }

    pub fn with_connect_timeout(mut self, timeout: Duration) -> Client {
        self.connect_timeout = timeout;
        self
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    fn connect(&self) -> Result<RpcClient<StreamCarrier<Wire>>, TallyOwlError> {
        let wire = dial(&self.address, self.connect_timeout, self.secure.as_ref())?;
        let carrier = StreamCarrier::with_max_frame(wire, self.max_frame_bytes).map_err(|e| {
            TallyOwlError::internal(format!("The connection could not be prepared: {e}"))
        })?;
        // Correlated batches pipeline on one connection, so every request
        // carries an id.
        Ok(RpcClient::new(carrier, true))
    }

    /// Invoke `service/op`. The reply carries its variant, so the caller can
    /// tell a typed result from a typed error.
    pub fn call(
        &self,
        service: &str,
        op: &str,
        payload: Vec<u8>,
    ) -> Result<RpcResponse, TallyOwlError> {
        let mut guard = self.inner.lock().expect("client lock");
        let mut last = String::new();
        for attempt in 0..2 {
            if guard.is_none() {
                *guard = Some(self.connect()?);
            }
            let client = guard.as_mut().expect("a connection exists");
            match client.call(service, op, payload.clone(), self.credential.clone()) {
                Ok(response) => return Ok(response),
                Err(e) => {
                    // The connection is now suspect either way. Drop it, so the
                    // retry runs on a fresh one and a later call does not
                    // inherit a half-read frame.
                    *guard = None;
                    last = e.to_string();
                    if attempt == 1 {
                        break;
                    }
                }
            }
        }
        Err(TallyOwlError::unavailable(format!(
            "We could not reach {} for `{op}`. {last}",
            self.address
        )))
    }

    /// Forget the current connection. The next call opens a fresh one.
    pub fn disconnect(&self) {
        *self.inner.lock().expect("client lock") = None;
    }
}

/// A pipelining client: several correlated calls outstanding on one connection.
///
/// `Client::call` sends one request and waits for its reply, so a caller with
/// one worker is bounded by the round trip rather than by the service. This
/// type separates the two halves, so a caller can have a window of calls in
/// flight and collect the replies as they arrive.
///
/// **A reply may arrive out of order.** `send` gives back the correlation ID
/// and `recv` reports which ID it answered. `docs/DELIVERY.md` section 3 permits
/// this, and a stable batch ID is what makes it safe: final storage
/// deduplicates, so a retry after a lost connection stays one logical commit.
///
/// This type is deliberately single threaded. It alternates between sending and
/// receiving on one carrier, which needs no background thread and no shared
/// state, and the window bound is what stops it from sending without limit.
pub struct Pipeline {
    address: String,
    credential: Option<String>,
    connect_timeout: Duration,
    max_frame_bytes: usize,
    window: usize,
    secure: Option<tls::Secure>,
    carrier: Option<StreamCarrier<Wire>>,
    next_id: u64,
    in_flight: usize,
}

impl Pipeline {
    /// A pipeline to `address` that keeps at most `window` calls outstanding.
    pub fn new(address: impl Into<String>, max_frame_bytes: usize, window: usize) -> Pipeline {
        Pipeline {
            address: address.into(),
            credential: None,
            connect_timeout: Duration::from_secs(5),
            max_frame_bytes,
            window: window.max(1),
            secure: None,
            carrier: None,
            next_id: 1,
            in_flight: 0,
        }
    }

    /// A pipeline over mutual TLS.
    ///
    /// This is the shape the replicated write path uses: a leader keeps several
    /// replication calls outstanding to each follower, and the overlap is the
    /// point. Before L058 was fixed there was no such thing, and measuring
    /// replication would have measured the carrier.
    pub fn secure(
        address: impl Into<String>,
        max_frame_bytes: usize,
        window: usize,
        identity: tls::Identity,
    ) -> Result<Pipeline, TallyOwlError> {
        Ok(Pipeline {
            secure: Some(tls::Secure::new(identity)?),
            ..Pipeline::new(address, max_frame_bytes, window)
        })
    }

    /// Present this credential on every call.
    pub fn with_credential(mut self, credential: impl Into<String>) -> Pipeline {
        let credential = credential.into();
        self.credential = (!credential.is_empty()).then_some(credential);
        self
    }

    pub fn with_connect_timeout(mut self, timeout: Duration) -> Pipeline {
        self.connect_timeout = timeout;
        self
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    /// How many calls are outstanding.
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// The window this pipeline keeps.
    pub fn window(&self) -> usize {
        self.window
    }

    /// Whether another call fits inside the window.
    pub fn has_room(&self) -> bool {
        self.in_flight < self.window
    }

    /// Send one call and return its correlation ID, without waiting for a reply.
    ///
    /// This does not enforce the window. A caller that wants the window enforced
    /// calls `recv` until `has_room` reports true, which is what gives the
    /// caller the chance to do something with each reply.
    pub fn send(
        &mut self,
        service: &str,
        op: &str,
        payload: Vec<u8>,
    ) -> Result<u64, TallyOwlError> {
        if self.carrier.is_none() {
            self.carrier = Some(self.connect()?);
        }
        let id = self.next_id;
        let mut request = RpcRequest::new(service, op, payload).with_id(id);
        request.auth = self.credential.clone();
        let encoded = request.encode().map_err(|e| {
            TallyOwlError::internal(format!("The request could not be encoded: {e}"))
        })?;

        let carrier = self.carrier.as_mut().expect("a connection exists");
        if let Err(e) = carrier.send_frame(&encoded) {
            // Every outstanding call on this connection now has an unknown
            // fate. The caller retries them by their stable IDs.
            self.reset();
            return Err(TallyOwlError::unavailable(format!(
                "We could not reach {} for `{op}`. {e}",
                self.address
            )));
        }
        self.next_id += 1;
        self.in_flight += 1;
        Ok(id)
    }

    /// Wait for the next reply, whichever call it answers.
    ///
    /// Returns `Ok(None)` when nothing is outstanding.
    pub fn recv(&mut self) -> Result<Option<(u64, RpcResponse)>, TallyOwlError> {
        if self.in_flight == 0 {
            return Ok(None);
        }
        let address = self.address.clone();
        let Some(carrier) = self.carrier.as_mut() else {
            self.reset();
            return Err(TallyOwlError::unavailable(format!(
                "The connection to {address} closed with replies outstanding."
            )));
        };
        let frame = match carrier.recv_frame() {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                self.reset();
                return Err(TallyOwlError::unavailable(format!(
                    "{address} closed the connection with replies outstanding."
                )));
            }
            Err(e) => {
                self.reset();
                return Err(TallyOwlError::unavailable(format!(
                    "We lost the connection to {address}. {e}"
                )));
            }
        };
        let response = match RpcResponse::decode(&frame) {
            Ok(response) => response,
            Err(e) => {
                self.reset();
                return Err(TallyOwlError::internal(format!(
                    "{address} sent a reply we could not read: {e}"
                )));
            }
        };
        self.in_flight -= 1;
        // A reply with no correlation ID cannot be matched to a call. That is a
        // protocol failure rather than an application error, so the connection
        // does not continue.
        let Some(id) = response.id else {
            self.reset();
            return Err(TallyOwlError::internal(format!(
                "{address} sent a reply with no correlation ID."
            )));
        };
        Ok(Some((id, response)))
    }

    /// Drop the connection and forget what was outstanding. The caller owns
    /// retrying those calls; a stable batch ID is what makes that safe.
    pub fn reset(&mut self) {
        self.carrier = None;
        self.in_flight = 0;
    }

    fn connect(&self) -> Result<StreamCarrier<Wire>, TallyOwlError> {
        let wire = dial(&self.address, self.connect_timeout, self.secure.as_ref())?;
        StreamCarrier::with_max_frame(wire, self.max_frame_bytes).map_err(|e| {
            TallyOwlError::internal(format!("The connection could not be prepared: {e}"))
        })
    }
}

/// Open one connection, plain or secured.
///
/// A secured connection finishes its handshake here rather than on the first
/// frame, so a peer that has no enrolled identity is refused at connect time and
/// the caller learns it from the call that opened the connection.
fn dial(
    address: &str,
    connect_timeout: Duration,
    secure: Option<&tls::Secure>,
) -> Result<Wire, TallyOwlError> {
    let target = address
        .to_socket_addrs()
        .map_err(|e| {
            TallyOwlError::new(
                ErrorCode::InvalidArgument,
                format!("The address `{address}` could not be read: {e}"),
            )
        })?
        .next()
        .ok_or_else(|| {
            TallyOwlError::new(
                ErrorCode::InvalidArgument,
                format!("The address `{address}` names no host."),
            )
        })?;
    let stream = TcpStream::connect_timeout(&target, connect_timeout)
        .map_err(|e| TallyOwlError::unavailable(format!("We could not reach {address}. {e}")))?;
    stream.set_nodelay(true).ok();
    match secure {
        None => Ok(Wire::Plain(stream)),
        Some(secure) => secure.connect(stream).map(Wire::Secure),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_FRAME: usize = 4 * 1024 * 1024;

    fn echo_server() -> Server {
        serve(
            "127.0.0.1:0",
            Arc::new(|request: &RpcRequest| match request.op.as_str() {
                "echo" => reply("EchoResponse", request.payload.clone()),
                "fail" => error_outcome(vec![0xa0]),
                other => unknown_operation(&request.service, other),
            }) as Arc<dyn Dispatcher>,
            MAX_FRAME,
        )
        .expect("serve")
    }

    #[test]
    fn a_credential_travels_on_every_call_of_a_connection() {
        // The whole tenancy model rests on this. A client that held a
        // credential and did not send it would take whatever the service used
        // as a default, which is somebody else's project.
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let server = serve(
            "127.0.0.1:0",
            Arc::new(move |request: &RpcRequest| {
                recorder.lock().unwrap().push(request.auth.clone());
                reply("EchoResponse", Vec::new())
            }) as Arc<dyn Dispatcher>,
            MAX_FRAME,
        )
        .expect("serve");

        let client =
            Client::new(server.local_address().to_string(), MAX_FRAME).with_credential("tow_abc");
        client.call("S", "echo", Vec::new()).unwrap();
        client.call("S", "echo", Vec::new()).unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Some("tow_abc".to_string()), Some("tow_abc".to_string())]
        );
    }

    #[test]
    fn a_client_with_no_credential_sends_none_rather_than_an_empty_one() {
        // An empty credential and no credential must not read differently at a
        // service, or a blank setting would become a distinct identity.
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let server = serve(
            "127.0.0.1:0",
            Arc::new(move |request: &RpcRequest| {
                recorder.lock().unwrap().push(request.auth.clone());
                reply("EchoResponse", Vec::new())
            }) as Arc<dyn Dispatcher>,
            MAX_FRAME,
        )
        .expect("serve");

        Client::new(server.local_address().to_string(), MAX_FRAME)
            .with_credential("")
            .call("S", "echo", Vec::new())
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![None]);
    }

    #[test]
    fn a_call_reaches_a_handler_and_comes_back_typed() {
        let server = echo_server();
        let client = Client::new(server.local_address().to_string(), MAX_FRAME);
        let response = client
            .call("TallyOwlCollector", "echo", vec![1, 2, 3])
            .expect("call");
        assert_eq!(response.status, Status::Ok);
        assert_eq!(response.variant.as_deref(), Some("EchoResponse"));
        assert_eq!(response.payload, vec![1, 2, 3]);
    }

    #[test]
    fn an_application_error_arrives_with_transport_status_zero() {
        // The rule this crate exists to hold. A caller must be able to tell a
        // rejected request from an unreachable service.
        let server = echo_server();
        let client = Client::new(server.local_address().to_string(), MAX_FRAME);
        let response = client
            .call("TallyOwlCollector", "fail", vec![])
            .expect("call");
        assert_eq!(response.status, Status::Ok);
        assert_eq!(response.variant.as_deref(), Some(SERVICE_ERROR_VARIANT));
    }

    #[test]
    fn an_unknown_operation_is_a_transport_failure() {
        let server = echo_server();
        let client = Client::new(server.local_address().to_string(), MAX_FRAME);
        let failure = client
            .call("TallyOwlCollector", "invent", vec![])
            .expect_err("an unknown operation has no typed reply");
        assert_eq!(failure.code, ErrorCode::Unavailable);
    }

    #[test]
    fn one_connection_carries_many_calls() {
        let server = echo_server();
        let client = Client::new(server.local_address().to_string(), MAX_FRAME);
        for n in 0..20u8 {
            let response = client
                .call("TallyOwlCollector", "echo", vec![n])
                .expect("call");
            assert_eq!(response.payload, vec![n]);
        }
    }

    #[test]
    fn an_unreachable_service_returns_a_retryable_error_that_names_the_address() {
        // Port 1 on the loopback refuses without waiting.
        let client = Client::new("127.0.0.1:1", MAX_FRAME);
        let failure = client
            .call("TallyOwlCollector", "echo", vec![])
            .unwrap_err();
        assert_eq!(failure.code, ErrorCode::Unavailable);
        assert!(failure.retryable, "an unreachable peer can come back");
        assert!(failure.message.contains("127.0.0.1:1"));
    }

    #[test]
    fn a_pipeline_keeps_several_calls_outstanding_on_one_connection() {
        let server = echo_server();
        let mut pipeline = Pipeline::new(server.local_address().to_string(), MAX_FRAME, 4);

        let mut sent = Vec::new();
        for n in 0..4u8 {
            sent.push(pipeline.send("S", "echo", vec![n]).expect("send"));
        }
        assert_eq!(pipeline.in_flight(), 4);
        assert!(!pipeline.has_room(), "the window is full");

        let mut answered = Vec::new();
        while let Some((id, response)) = pipeline.recv().expect("recv") {
            assert_eq!(response.status, Status::Ok);
            answered.push(id);
        }
        answered.sort_unstable();
        assert_eq!(answered, sent, "every call was answered exactly once");
        assert_eq!(pipeline.in_flight(), 0);
    }

    #[test]
    fn a_reply_reaches_the_call_it_answers_even_when_a_slow_one_is_ahead_of_it() {
        // This is the property that makes pipelining worth anything. The first
        // call takes 300 ms, and the second must not wait behind it.
        let server = serve(
            "127.0.0.1:0",
            Arc::new(|request: &RpcRequest| {
                if request.payload == b"slow".to_vec() {
                    std::thread::sleep(Duration::from_millis(300));
                }
                reply("EchoResponse", request.payload.clone())
            }) as Arc<dyn Dispatcher>,
            MAX_FRAME,
        )
        .expect("serve");

        let mut pipeline = Pipeline::new(server.local_address().to_string(), MAX_FRAME, 4);
        let slow = pipeline.send("S", "echo", b"slow".to_vec()).expect("send");
        let fast = pipeline.send("S", "echo", b"f".to_vec()).expect("send");

        let started = std::time::Instant::now();
        let (first_id, _) = pipeline.recv().expect("recv").expect("a reply");
        assert_eq!(first_id, fast, "the fast call is answered first");
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "the fast call waited behind the slow one: {:?}",
            started.elapsed()
        );

        let (second_id, _) = pipeline.recv().expect("recv").expect("a reply");
        assert_eq!(second_id, slow);
    }

    #[test]
    fn a_connection_that_serves_one_at_a_time_keeps_its_order() {
        // A request with no correlation ID cannot be matched to a reply, so it
        // must never be answered out of order. `RpcClient` with `multiplexed`
        // false sends no ID, and this asserts the server honours that.
        let server = serve(
            "127.0.0.1:0",
            Arc::new(|request: &RpcRequest| {
                assert!(request.id.is_none());
                reply("EchoResponse", request.payload.clone())
            }) as Arc<dyn Dispatcher>,
            MAX_FRAME,
        )
        .expect("serve");

        let stream = TcpStream::connect(server.local_address()).expect("connect");
        let carrier = StreamCarrier::with_max_frame(stream, MAX_FRAME).expect("carrier");
        let mut client = RpcClient::new(carrier, false);
        for n in 0..5u8 {
            let response = client.call("S", "echo", vec![n], None).expect("call");
            assert_eq!(response.payload, vec![n]);
            assert_eq!(response.id, None);
        }
    }

    #[test]
    fn a_pipeline_reports_a_lost_connection_rather_than_hanging() {
        let mut pipeline = Pipeline::new("127.0.0.1:1", MAX_FRAME, 4);
        let failure = pipeline
            .send("S", "echo", vec![1])
            .expect_err("port 1 refuses");
        assert_eq!(failure.code, ErrorCode::Unavailable);
        assert!(failure.retryable);
        assert_eq!(pipeline.in_flight(), 0);
    }

    #[test]
    fn a_pipeline_with_nothing_outstanding_returns_none_rather_than_blocking() {
        let server = echo_server();
        let mut pipeline = Pipeline::new(server.local_address().to_string(), MAX_FRAME, 4);
        assert_eq!(pipeline.recv().expect("recv"), None);
    }

    #[test]
    fn a_pipelined_application_error_still_arrives_with_transport_status_zero() {
        // The rule this crate exists to hold, on the pipelined path too.
        let server = echo_server();
        let mut pipeline = Pipeline::new(server.local_address().to_string(), MAX_FRAME, 4);
        let id = pipeline.send("S", "fail", vec![]).expect("send");
        let (answered, response) = pipeline.recv().expect("recv").expect("a reply");
        assert_eq!(answered, id);
        assert_eq!(response.status, Status::Ok);
        assert_eq!(response.variant.as_deref(), Some(SERVICE_ERROR_VARIANT));
    }

    #[test]
    fn a_connection_serves_no_more_than_its_in_flight_bound() {
        // The bound is what stops a connection turning a burst into unbounded
        // work. Six calls arrive at once and no more than two ever overlap.
        let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&live);
        let high_water = Arc::clone(&peak);
        let server = serve_with_in_flight(
            "127.0.0.1:0",
            Arc::new(move |request: &RpcRequest| {
                let now = counter.fetch_add(1, Ordering::SeqCst) + 1;
                high_water.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(60));
                counter.fetch_sub(1, Ordering::SeqCst);
                reply("EchoResponse", request.payload.clone())
            }) as Arc<dyn Dispatcher>,
            MAX_FRAME,
            2,
        )
        .expect("serve");

        let mut pipeline = Pipeline::new(server.local_address().to_string(), MAX_FRAME, 6);
        for n in 0..6u8 {
            pipeline.send("S", "echo", vec![n]).expect("send");
        }
        while pipeline.recv().expect("recv").is_some() {}
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "the bound was exceeded: {} overlapped",
            peak.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn a_frame_over_the_limit_never_reaches_memory() {
        let server = serve(
            "127.0.0.1:0",
            Arc::new(|r: &RpcRequest| reply("EchoResponse", r.payload.clone()))
                as Arc<dyn Dispatcher>,
            1024,
        )
        .expect("serve");
        let client = Client::new(server.local_address().to_string(), 8 * 1024 * 1024);
        let failure = client
            .call("S", "echo", vec![0u8; 64 * 1024])
            .expect_err("the service refuses the frame");
        assert_eq!(failure.code, ErrorCode::Unavailable);
    }
}
