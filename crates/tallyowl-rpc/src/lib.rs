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
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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

    /// This runs from a drop guard while a worker unwinds, so it must not panic
    /// itself. Nothing that holds this lock can fail, so a poisoned lock still
    /// guards a sound count.
    fn release(&self) {
        *self.free.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.ready.notify_one();
    }
}

/// Gives a permit back when a worker ends, however it ends.
///
/// A worker that released its permit on its last line leaked it when the handler
/// panicked. Eight such requests stopped a connection's read loop for good.
struct PermitGuard {
    permits: Arc<Permits>,
    stats: Arc<ServerStats>,
}

impl Drop for PermitGuard {
    fn drop(&mut self) {
        self.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
        self.permits.release();
    }
}

/// What a listener is doing, for a host that publishes it as metrics.
///
/// These are plain counts with no labels. A peer address or a credential here
/// would be a cardinality problem in the operator's own monitoring.
#[derive(Default)]
pub struct ServerStats {
    open_connections: AtomicUsize,
    in_flight: AtomicUsize,
    refused_connections: AtomicU64,
    handler_panics: AtomicU64,
    idle_closed: AtomicU64,
    handshakes_refused: AtomicU64,
}

impl ServerStats {
    /// Connections open now.
    pub fn open_connections(&self) -> usize {
        self.open_connections.load(Ordering::Relaxed)
    }

    /// Correlated requests a handler is working on now.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Connections refused because the listener was at its connection limit.
    pub fn refused_connections(&self) -> u64 {
        self.refused_connections.load(Ordering::Relaxed)
    }

    /// Requests whose handler panicked. Each one was answered with an error.
    pub fn handler_panics(&self) -> u64 {
        self.handler_panics.load(Ordering::Relaxed)
    }

    /// Connections this listener refused because the peer failed the TLS
    /// handshake: a certificate no trusted authority signed, a plaintext
    /// client, or a protocol the listener does not speak. A connection that
    /// came before this node had an identity is not counted here.
    pub fn handshakes_refused(&self) -> u64 {
        self.handshakes_refused.load(Ordering::Relaxed)
    }

    /// Connections closed because the peer sent nothing for the idle period.
    pub fn idle_closed(&self) -> u64 {
        self.idle_closed.load(Ordering::Relaxed)
    }
}

/// Closes the count of one open connection when its thread ends.
struct ConnectionGuard(Arc<ServerStats>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.open_connections.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Told about a handler that panicked: the service, the operation, and what the
/// panic said. A host logs it. The listener has already answered the caller.
pub type PanicHook = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

/// How a listener behaves at its trust boundary.
///
/// `serve` keeps the behaviour it always had: no connection limit and no idle
/// deadline. A listener that faces applications sets both, because a peer that
/// opens a socket and sends nothing otherwise holds a thread for as long as it
/// likes.
#[derive(Clone)]
pub struct ServerOptions {
    max_frame_bytes: usize,
    max_in_flight: usize,
    max_connections: Option<usize>,
    idle_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    on_panic: Option<PanicHook>,
    allow_anonymous: bool,
    on_handshake_refused: Option<HandshakeHook>,
}

/// Called once for each connection a listener refuses during its TLS handshake.
pub type HandshakeHook = Arc<dyn Fn() + Send + Sync>;

impl ServerOptions {
    pub fn new(max_frame_bytes: usize) -> ServerOptions {
        ServerOptions {
            max_frame_bytes,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            max_connections: None,
            idle_timeout: None,
            write_timeout: Some(DEFAULT_IO_TIMEOUT),
            on_panic: None,
            allow_anonymous: false,
            on_handshake_refused: None,
        }
    }

    /// How many correlated requests one connection serves at the same time.
    pub fn max_in_flight(mut self, max_in_flight: usize) -> ServerOptions {
        self.max_in_flight = max_in_flight.max(1);
        self
    }

    /// How many connections may be open at one time. Zero means no limit.
    pub fn max_connections(mut self, max_connections: usize) -> ServerOptions {
        self.max_connections = (max_connections > 0).then_some(max_connections);
        self
    }

    /// Close a connection that sends nothing for this long and has no request in
    /// progress. A reconnecting client opens a new one on its next call. Zero
    /// means no deadline.
    pub fn idle_timeout(mut self, idle_timeout: Duration) -> ServerOptions {
        self.idle_timeout = (!idle_timeout.is_zero()).then_some(idle_timeout);
        self
    }

    /// How long one socket write of a reply may wait on a peer that is not
    /// reading. Zero means no deadline.
    pub fn write_timeout(mut self, write_timeout: Duration) -> ServerOptions {
        self.write_timeout = (!write_timeout.is_zero()).then_some(write_timeout);
        self
    }

    /// Call this when a handler panics.
    pub fn on_panic(mut self, hook: PanicHook) -> ServerOptions {
        self.on_panic = Some(hook);
        self
    }

    /// On a mutual TLS listener, also accept a client that shows no
    /// certificate, as `Peer::Anonymous`. A client that shows one is still
    /// verified, and a certificate that does not verify is still refused.
    ///
    /// This is how a node that has no identity yet reaches `enroll-node`, and
    /// how an operator's client, which proves itself with a session token,
    /// reaches the control operations. Each operation that needs a proved node
    /// must then refuse `Anonymous` itself. See D62.
    pub fn allow_anonymous(mut self, allow: bool) -> ServerOptions {
        self.allow_anonymous = allow;
        self
    }

    pub(crate) fn anonymous_allowed(&self) -> bool {
        self.allow_anonymous
    }

    /// Call this for each connection refused during the TLS handshake, so a
    /// service can count it under its own listener's name. A refusal is not
    /// logged here: a stranger who opens connections all day would write a
    /// line for each one.
    pub fn on_handshake_refused(mut self, hook: HandshakeHook) -> ServerOptions {
        self.on_handshake_refused = Some(hook);
        self
    }
}

/// A server-side stream that remembers a passed deadline and counts what it
/// read, so the read loop can tell an idle connection from a broken one.
struct Watched<S> {
    inner: S,
    watch: Arc<Watch>,
}

#[derive(Default)]
struct Watch {
    timed_out: AtomicBool,
    bytes_read: AtomicU64,
}

impl<S: Read> Read for Watched<S> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self.inner.read(buffer) {
            Ok(read) => {
                self.watch
                    .bytes_read
                    .fetch_add(read as u64, Ordering::Relaxed);
                Ok(read)
            }
            Err(e) => {
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) {
                    self.watch.timed_out.store(true, Ordering::Relaxed);
                }
                Err(e)
            }
        }
    }
}

impl<S: Write> Write for Watched<S> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// What a service does with one decoded request.
///
/// The implementation is normally a match over `request.op` that decodes, calls
/// a typed handler, and encodes. `docs/DELIVERY.md` decides what each arm does;
/// this trait only decides where it lives.
pub trait Dispatcher: Send + Sync + 'static {
    fn dispatch(&self, request: &RpcRequest) -> HandlerOutcome;

    /// The peer-aware entry point. Every listener calls this, with the peer
    /// the transport proved. The default ignores the peer, so every dispatcher
    /// written before D62 keeps working unchanged.
    fn dispatch_from(&self, request: &RpcRequest, peer: &Peer) -> HandlerOutcome {
        let _ = peer;
        self.dispatch(request)
    }
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

pub mod address;
pub mod duplex;
pub mod material;
pub mod peer;
pub mod tls;
pub mod trust;

pub use address::{Address, Socket};
pub use peer::{Peer, PeerIdentity};

/// What a connection is carried on.
///
/// A plain socket and a TLS session behave the same above the framing, so the
/// client, the pipeline, and the server loop are written once and take either.
/// The enum rather than a boxed trait object keeps the read path free of a
/// virtual call for each frame.
pub enum Wire {
    Plain(Socket),
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

/// How long a client waits for one socket read or one socket write.
///
/// A connect timeout alone is not a deadline. A peer that accepts the connection
/// and then never answers holds the caller, and every lock the caller holds, for
/// as long as the peer likes. This bound is on by default, so a caller that never
/// heard of it is still protected.
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// A client's connection, which remembers that a deadline passed.
///
/// The framing layer turns an I/O error into text, and the text of a timeout is
/// the operating system's. Reading the error kind here is what lets a caller be
/// told "the peer did not answer in time" rather than "resource temporarily
/// unavailable".
pub struct ClientWire {
    wire: Wire,
    timed_out: bool,
    /// The TLS failure the connection ended on, if it ended on one. In TLS 1.3
    /// a server refuses a client certificate after the client has finished its
    /// handshake, so the refusal arrives on the first read. The framing turns
    /// it into text, and this keeps its type.
    tls: Option<rustls::Error>,
}

/// Why a client connection ended, read after the fact.
#[derive(Default)]
struct Ending {
    timed_out: bool,
    tls: Option<rustls::Error>,
}

impl ClientWire {
    fn new(wire: Wire) -> ClientWire {
        ClientWire {
            wire,
            timed_out: false,
            tls: None,
        }
    }

    fn note<T>(&mut self, result: std::io::Result<T>) -> std::io::Result<T> {
        if let Err(e) = &result {
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) {
                self.timed_out = true;
            }
            if let Some(tls) = duplex::rustls_error(e) {
                self.tls = Some(tls.clone());
            }
        }
        result
    }

    fn ending(self) -> Ending {
        Ending {
            timed_out: self.timed_out,
            tls: self.tls,
        }
    }
}

/// A connection that ended on a TLS failure fails the same way on the next
/// attempt, so it is reported with the handshake's words.
fn tls_ending(
    address: &str,
    secure: Option<&tls::Secure>,
    ending: &Ending,
) -> Option<TallyOwlError> {
    let tls = ending.tls.clone()?;
    let target = Address::parse(address).ok()?;
    let server_name = secure.map(tls::Secure::server_name).unwrap_or_default();
    Some(tls::handshake_failure(
        &target,
        server_name,
        &std::io::Error::other(tls),
    ))
}

impl Read for ClientWire {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let result = self.wire.read(buffer);
        self.note(result)
    }
}

impl Write for ClientWire {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let result = self.wire.write(buffer);
        self.note(result)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let result = self.wire.flush();
        self.note(result)
    }
}

/// The error for a peer that did not answer before the I/O deadline.
///
/// It is `unavailable`, which is retryable: the peer may be slow rather than
/// gone, and the call carries a stable ID, so sending it again is safe.
fn timed_out(address: &str, op: &str, io_timeout: Option<Duration>) -> TallyOwlError {
    let waited = io_timeout
        .map(|t| format!("{} ms", t.as_millis()))
        .unwrap_or_else(|| "the time allowed".to_string());
    TallyOwlError::unavailable(format!(
        "{address} did not answer `{op}` within {waited}. We closed the connection, and the next call opens a new one."
    ))
}

/// What a failed call means to the caller.
///
/// A frame the peer could not read, and a frame too large to send, fail the same
/// way on every attempt. Calling either one `unavailable` makes a caller retry a
/// permanent failure for as long as its retry policy lasts.
fn call_failure(
    address: &str,
    op: &str,
    error: &csilgen_transport::TransportError,
) -> TallyOwlError {
    use csilgen_transport::TransportError;
    match error {
        TransportError::Status { code, .. } if *code == Status::MalformedEnvelope.code() => {
            TallyOwlError::invalid_argument(format!(
                "{address} could not read the `{op}` request. It will not read it on another attempt either. {error}"
            ))
        }
        // The framing does not say which direction carried the frame. A
        // request that is too large and a TLS listener answering a plaintext
        // client (its first bytes read as an enormous length) both land here,
        // and neither goes better on another attempt.
        TransportError::FrameTooLarge { got, max } => TallyOwlError::invalid_argument(format!(
            "A frame of {got} bytes for `{op}` is larger than the {max} bytes this connection carries. If the request is large, send less in one call. If {address} serves TLS, connect to it with TLS."
        )),
        _ => TallyOwlError::unavailable(format!(
            "We could not reach {address} for `{op}`. {error}"
        )),
    }
}

/// A running CSIL-RPC listener.
pub struct Server {
    local_address: std::net::SocketAddr,
    /// The address a client uses: the TCP address with the port the operating
    /// system chose, or the unix socket path.
    bound: Address,
    stopping: Arc<AtomicBool>,
    stats: Arc<ServerStats>,
}

impl Server {
    /// The address a client uses to reach this listener. For a unix socket this
    /// is `unix:<path>`; [`Server::local_address`] has no path to give.
    pub fn bound(&self) -> &Address {
        &self.bound
    }

    /// What this listener is doing, for a host that publishes it.
    pub fn stats(&self) -> Arc<ServerStats> {
        Arc::clone(&self.stats)
    }

    /// Wait until no request is in progress, or until `limit` has passed.
    /// Returns whether the listener went quiet.
    ///
    /// A host calls this after `stop`, so a request that was already accepted is
    /// answered before the process exits.
    pub fn wait_until_quiet(&self, limit: Duration) -> bool {
        let started = std::time::Instant::now();
        while self.stats.in_flight() > 0 {
            if started.elapsed() >= limit {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// The TCP address bound. A unix socket has none and reports the
    /// unspecified address; [`Server::bound`] names its path.
    pub fn local_address(&self) -> std::net::SocketAddr {
        self.local_address
    }

    /// Stop accepting. An open connection finishes the frame it is serving.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        // Wake the accept loop so it observes the flag rather than waiting for
        // the next real connection.
        let _ = address::connect(&self.bound, Duration::from_secs(1));
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
        // A socket file that nothing serves would make the next bind check
        // for a live server first. Remove it while it is certainly this one's.
        if let Address::Unix(path) = &self.bound {
            let _ = std::fs::remove_file(path);
        }
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
    serve_with(
        address,
        dispatcher,
        ServerOptions::new(max_frame_bytes).max_in_flight(max_in_flight),
    )
}

/// Serve CSIL-RPC with every listener option stated. The connections are
/// plaintext: the peer is `Local` on a loopback or unix address and
/// `Unverified` anywhere else. `address` may be `unix:<path>`.
pub fn serve_with(
    address: &str,
    dispatcher: Arc<dyn Dispatcher>,
    options: ServerOptions,
) -> std::io::Result<Server> {
    let address = Address::parse(address)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.message))?;
    serve_on(&address, dispatcher, options, tls::Security::Plain)
}

/// Serve over TLS. The error names the address when the bind fails.
pub(crate) fn serve_secured(
    address: &str,
    dispatcher: Arc<dyn Dispatcher>,
    options: ServerOptions,
    security: tls::Security,
) -> Result<Server, TallyOwlError> {
    let address = Address::parse(address)?;
    serve_on(&address, dispatcher, options, security).map_err(|e| {
        TallyOwlError::new(
            ErrorCode::FailedPrecondition,
            format!("{address} could not be bound: {e}"),
        )
    })
}

/// The one accept loop, for every kind of security and every kind of address.
fn serve_on(
    address: &Address,
    dispatcher: Arc<dyn Dispatcher>,
    options: ServerOptions,
    security: tls::Security,
) -> std::io::Result<Server> {
    let listener = address::Listener::bind(address)?;
    let local_address = listener.local_address()?;
    let bound = listener.reachable(address)?;
    let plain_peer = if bound.plaintext_permitted() {
        Peer::Local
    } else {
        Peer::Unverified
    };
    let stopping = Arc::new(AtomicBool::new(false));
    let loop_stopping = Arc::clone(&stopping);
    let stats = Arc::new(ServerStats::default());
    let loop_stats = Arc::clone(&stats);
    let security = Arc::new(security);

    std::thread::Builder::new()
        .name("tallyowl-rpc-accept".into())
        .spawn(move || loop {
            let accepted = listener.accept();
            if loop_stopping.load(Ordering::Relaxed) {
                break;
            }
            let Ok(socket) = accepted else { continue };
            // Refused before a handshake, which is the expensive part.
            let Some(guard) = admit(&socket, &options, &loop_stats) else {
                continue;
            };
            let dispatcher = Arc::clone(&dispatcher);
            let connection_stopping = Arc::clone(&loop_stopping);
            let options = options.clone();
            let stats = Arc::clone(&loop_stats);
            let security = Arc::clone(&security);
            let plain_peer = plain_peer.clone();
            // A host that will not give us a thread drops the connection, and
            // the guard gives the count back.
            let _ = std::thread::Builder::new()
                .name("tallyowl-rpc-connection".into())
                .spawn(move || {
                    let _guard = guard;
                    let (read_side, write_side, peer) = match security.as_ref() {
                        tls::Security::Plain => {
                            let Ok(write_side) = socket.try_clone() else {
                                return;
                            };
                            (Wire::Plain(socket), Wire::Plain(write_side), plain_peer)
                        }
                        secured => match secured.accept(socket) {
                            Ok((duplex, peer)) => {
                                (Wire::Secure(duplex.clone()), Wire::Secure(duplex), peer)
                            }
                            Err(tls::Refusal::NoIdentity) => return,
                            Err(tls::Refusal::Handshake) => {
                                stats.handshakes_refused.fetch_add(1, Ordering::Relaxed);
                                if let Some(hook) = &options.on_handshake_refused {
                                    hook();
                                }
                                return;
                            }
                        },
                    };
                    serve_connection(
                        read_side,
                        write_side,
                        dispatcher,
                        &options,
                        stats,
                        connection_stopping,
                        Arc::new(peer),
                    );
                });
        })?;

    Ok(Server {
        local_address,
        bound,
        stopping,
        stats,
    })
}

/// Count one accepted connection, or refuse it when the listener is full.
///
/// The deadlines go on the socket here, before anything wraps it, so they cover
/// a TLS handshake as well as every frame.
pub(crate) fn admit(
    stream: &Socket,
    options: &ServerOptions,
    stats: &Arc<ServerStats>,
) -> Option<ConnectionGuard> {
    let open = stats.open_connections.fetch_add(1, Ordering::Relaxed) + 1;
    let guard = ConnectionGuard(Arc::clone(stats));
    if options.max_connections.is_some_and(|limit| open > limit) {
        // Closing without a word is the honest answer here. A reply would need a
        // frame the peer has not asked for, and on the secured path it would
        // need a handshake, which is the cost this limit exists to refuse.
        stats.refused_connections.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    stream.set_nodelay();
    if stream.set_read_timeout(options.idle_timeout).is_err()
        || stream.set_write_timeout(options.write_timeout).is_err()
    {
        return None;
    }
    Some(guard)
}

/// What a panic said, for the log. A panic carries a `&str` or a `String` in
/// every case this code base produces.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "the panic carried no message".to_string())
}

/// Run one handler, and turn a panic into an answer.
///
/// A handler that panicked used to take its thread with it: no reply was
/// written, so the caller waited for ever, and the caller's resend of the same
/// stable batch met the same panic. The caller now gets `internal` against its
/// own request, the host is told, and the connection keeps serving.
fn dispatch_contained(
    dispatcher: &Arc<dyn Dispatcher>,
    request: &RpcRequest,
    peer: &Peer,
    options: &ServerOptions,
    stats: &ServerStats,
) -> HandlerOutcome {
    match catch_unwind(AssertUnwindSafe(|| dispatcher.dispatch_from(request, peer))) {
        Ok(outcome) => outcome,
        Err(payload) => {
            stats.handler_panics.fetch_add(1, Ordering::Relaxed);
            if let Some(hook) = &options.on_panic {
                hook(&request.service, &request.op, &panic_text(payload.as_ref()));
            }
            HandlerOutcome::Transport(
                Status::Internal,
                format!(
                    "This service failed while it handled `{}`. The failure is in its log.",
                    request.op
                ),
            )
        }
    }
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
    options: &ServerOptions,
    stats: Arc<ServerStats>,
    stopping: Arc<AtomicBool>,
    peer: Arc<Peer>,
) where
    S: Read + Write + Send + 'static,
{
    let watch = Arc::new(Watch::default());
    let read_side = Watched {
        inner: read_side,
        watch: Arc::clone(&watch),
    };
    // A carrier with a host-chosen limit refuses an oversized frame before it
    // allocates for it, so a decompression bomb never reaches memory.
    let Ok(mut reader) = StreamCarrier::with_max_frame(read_side, options.max_frame_bytes) else {
        return;
    };
    let Ok(writer) = StreamCarrier::with_max_frame(write_side, options.max_frame_bytes) else {
        return;
    };
    let writer = Arc::new(Mutex::new(writer));
    let permits = Arc::new(Permits::new(options.max_in_flight));

    while !stopping.load(Ordering::Relaxed) {
        // A clean end of stream, or a carrier that failed. Either way this
        // connection is finished.
        let read_before = watch.bytes_read.load(Ordering::Relaxed);
        let frame = match reader.recv_frame() {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(_) => {
                let waited_out = watch.timed_out.swap(false, Ordering::Relaxed);
                let mid_frame = watch.bytes_read.load(Ordering::Relaxed) != read_before;
                // The idle deadline passed with a request still in progress and
                // no part of a new frame read. The peer is waiting for that
                // reply, so this is not an idle connection. Any byte of a frame
                // read means the stream position is lost, and that closes it.
                if waited_out && !mid_frame && stats.in_flight() > 0 {
                    continue;
                }
                if waited_out {
                    stats.idle_closed.fetch_add(1, Ordering::Relaxed);
                }
                break;
            }
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
                let outcome = dispatch_contained(&dispatcher, &request, &peer, options, &stats);
                let reply = outcome_to_reply(outcome, None);
                if !write_reply(&writer, &reply) {
                    break;
                }
            }
            Some(id) => {
                permits.acquire();
                stats.in_flight.fetch_add(1, Ordering::Relaxed);
                let guard = PermitGuard {
                    permits: Arc::clone(&permits),
                    stats: Arc::clone(&stats),
                };
                let worker_dispatcher = Arc::clone(&dispatcher);
                let worker_writer = Arc::clone(&writer);
                let worker_options = options.clone();
                let worker_stats = Arc::clone(&stats);
                let worker_peer = Arc::clone(&peer);
                let started = std::thread::Builder::new()
                    .name("tallyowl-rpc-serve".into())
                    .spawn(move || {
                        // The permit goes back when this thread ends, however it
                        // ends.
                        let _guard = guard;
                        let outcome = dispatch_contained(
                            &worker_dispatcher,
                            &request,
                            &worker_peer,
                            &worker_options,
                            &worker_stats,
                        );
                        write_reply(&worker_writer, &outcome_to_reply(outcome, Some(id)));
                    });
                if started.is_err() {
                    // The host would not give us a thread. The closure was
                    // dropped, and the permit went back with it. Say so against
                    // this request's ID rather than leaving the caller waiting
                    // for a reply that is never coming.
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
    /// The deadline for one socket read or write. `None` waits without limit.
    io_timeout: Option<Duration>,
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
    inner: Mutex<Option<RpcClient<StreamCarrier<ClientWire>>>>,
}

impl Client {
    pub fn new(address: impl Into<String>, max_frame_bytes: usize) -> Client {
        Client {
            address: address.into(),
            connect_timeout: Duration::from_secs(5),
            io_timeout: Some(DEFAULT_IO_TIMEOUT),
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

    /// A client over server-authenticated TLS: it checks that the server's
    /// certificate carries `server_name` and chains to `roots`, and it shows no
    /// certificate of its own. `roots: None` means the roots of the operating
    /// system. See D62.
    pub fn server_auth(
        address: impl Into<String>,
        max_frame_bytes: usize,
        server_name: &str,
        roots: Option<Vec<Vec<u8>>>,
    ) -> Result<Client, TallyOwlError> {
        Ok(Client {
            secure: Some(tls::Secure::server_auth(server_name, roots)?),
            ..Client::new(address, max_frame_bytes)
        })
    }

    /// A client over mutual TLS: it shows the current identity and checks the
    /// server against the current authorities, on each new connection.
    pub fn mutual(
        address: impl Into<String>,
        max_frame_bytes: usize,
        server_name: &str,
        identity: Arc<dyn material::IdentitySource>,
        trust: Arc<dyn trust::TrustSource>,
    ) -> Client {
        Client {
            secure: Some(tls::Secure::mutual(server_name, identity, trust)),
            ..Client::new(address, max_frame_bytes)
        }
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

    /// How long one socket read or one socket write may wait. The default is
    /// [`DEFAULT_IO_TIMEOUT`]. A zero duration waits without limit, which is
    /// only right for a caller that has its own deadline.
    pub fn with_io_timeout(mut self, timeout: Duration) -> Client {
        self.io_timeout = (!timeout.is_zero()).then_some(timeout);
        self
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    fn connect(&self) -> Result<RpcClient<StreamCarrier<ClientWire>>, TallyOwlError> {
        let wire = dial(
            &self.address,
            self.connect_timeout,
            self.io_timeout,
            self.secure.as_ref(),
        )?;
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
        let mut last = None;
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
                    let ending = guard
                        .take()
                        .map(|client| client.into_carrier().into_inner().ending())
                        .unwrap_or_default();
                    if let Some(failure) = tls_ending(&self.address, self.secure.as_ref(), &ending)
                    {
                        return Err(failure);
                    }
                    if ending.timed_out {
                        // A late reply on this connection would answer the wrong
                        // call, so it is never used again. A second attempt would
                        // double the wait against a peer that is not answering,
                        // so the caller decides.
                        return Err(timed_out(&self.address, op, self.io_timeout));
                    }
                    let failure = call_failure(&self.address, op, &e);
                    if !failure.retryable || attempt == 1 {
                        return Err(failure);
                    }
                    last = Some(failure);
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            TallyOwlError::unavailable(format!("We could not reach {} for `{op}`.", self.address))
        }))
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
    /// The deadline for one socket read or write. `None` waits without limit.
    io_timeout: Option<Duration>,
    max_frame_bytes: usize,
    window: usize,
    secure: Option<tls::Secure>,
    carrier: Option<StreamCarrier<ClientWire>>,
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
            io_timeout: Some(DEFAULT_IO_TIMEOUT),
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

    /// A pipeline over server-authenticated TLS. See [`Client::server_auth`].
    pub fn server_auth(
        address: impl Into<String>,
        max_frame_bytes: usize,
        window: usize,
        server_name: &str,
        roots: Option<Vec<Vec<u8>>>,
    ) -> Result<Pipeline, TallyOwlError> {
        Ok(Pipeline {
            secure: Some(tls::Secure::server_auth(server_name, roots)?),
            ..Pipeline::new(address, max_frame_bytes, window)
        })
    }

    /// A pipeline over mutual TLS with an identity and authorities that can
    /// change. See [`Client::mutual`].
    pub fn mutual(
        address: impl Into<String>,
        max_frame_bytes: usize,
        window: usize,
        server_name: &str,
        identity: Arc<dyn material::IdentitySource>,
        trust: Arc<dyn trust::TrustSource>,
    ) -> Pipeline {
        Pipeline {
            secure: Some(tls::Secure::mutual(server_name, identity, trust)),
            ..Pipeline::new(address, max_frame_bytes, window)
        }
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

    /// How long one socket read or one socket write may wait. The default is
    /// [`DEFAULT_IO_TIMEOUT`]. A zero duration waits without limit.
    ///
    /// For `recv` this is how long the pipeline waits for the next reply,
    /// whichever call it answers.
    pub fn with_io_timeout(mut self, timeout: Duration) -> Pipeline {
        self.io_timeout = (!timeout.is_zero()).then_some(timeout);
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
            let ending = self.reset_and_report();
            if let Some(failure) = tls_ending(&self.address, self.secure.as_ref(), &ending) {
                return Err(failure);
            }
            if ending.timed_out {
                return Err(timed_out(&self.address, op, self.io_timeout));
            }
            return Err(call_failure(&self.address, op, &e));
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
                let ending = self.reset_and_report();
                if let Some(failure) = tls_ending(&address, self.secure.as_ref(), &ending) {
                    return Err(failure);
                }
                if ending.timed_out {
                    return Err(timed_out(
                        &address,
                        "the outstanding calls",
                        self.io_timeout,
                    ));
                }
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

    /// Reset, and say why the connection ended: a deadline that passed, or a
    /// TLS failure.
    fn reset_and_report(&mut self) -> Ending {
        let ending = self
            .carrier
            .take()
            .map(|carrier| carrier.into_inner().ending())
            .unwrap_or_default();
        self.in_flight = 0;
        ending
    }

    fn connect(&self) -> Result<StreamCarrier<ClientWire>, TallyOwlError> {
        let wire = dial(
            &self.address,
            self.connect_timeout,
            self.io_timeout,
            self.secure.as_ref(),
        )?;
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
///
/// The read and write deadlines go on the socket before TLS wraps it. Every
/// handle on the connection shares the one socket, so the deadline covers the
/// handshake as well as every later frame.
fn dial(
    address: &str,
    connect_timeout: Duration,
    io_timeout: Option<Duration>,
    secure: Option<&tls::Secure>,
) -> Result<ClientWire, TallyOwlError> {
    let target = Address::parse(address)?;
    let socket = address::connect(&target, connect_timeout)?;
    socket.set_nodelay();
    for result in [
        socket.set_read_timeout(io_timeout),
        socket.set_write_timeout(io_timeout),
    ] {
        result.map_err(|e| {
            TallyOwlError::internal(format!(
                "The connection to {address} could not be given a deadline: {e}"
            ))
        })?;
    }
    match secure {
        None => Ok(ClientWire::new(Wire::Plain(socket))),
        Some(secure) => secure
            .connect(&target, socket)
            .map(|tls| ClientWire::new(Wire::Secure(tls))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

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

    /// A peer that accepts a connection and never answers, which is what a
    /// stalled process with a live socket looks like. It keeps each connection
    /// open, so the caller sees a silence and not a close.
    fn silent_peer() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address").to_string();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                counter.fetch_add(1, Ordering::SeqCst);
                held.push(stream);
            }
        });
        (address, accepted)
    }

    #[test]
    fn a_peer_that_never_answers_returns_a_retryable_error_rather_than_holding_the_caller() {
        // The wedge this guards against held a delivery thread, and the lock
        // every other caller of the same client needed, for two days.
        let (address, accepted) = silent_peer();
        let client =
            Client::new(address.clone(), MAX_FRAME).with_io_timeout(Duration::from_millis(40));
        let failure = client
            .call("S", "echo", vec![1])
            .expect_err("a silent peer is a failure");
        assert_eq!(failure.code, ErrorCode::Unavailable);
        assert!(failure.retryable, "the peer may only be slow");
        assert!(failure.message.contains(&address), "{}", failure.message);
        assert!(
            failure.message.contains("did not answer"),
            "{}",
            failure.message
        );
        // One attempt. A second one would double the wait against a peer that
        // is not answering.
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_connection_that_waited_out_its_deadline_is_never_used_again() {
        // A late reply on the old connection would answer the wrong call.
        let (address, accepted) = silent_peer();
        let client = Client::new(address, MAX_FRAME).with_io_timeout(Duration::from_millis(40));
        client.call("S", "echo", vec![1]).expect_err("first");
        client.call("S", "echo", vec![2]).expect_err("second");
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            2,
            "the second call did not open a connection of its own"
        );
    }

    #[test]
    fn a_pipeline_that_waits_out_its_deadline_forgets_what_was_outstanding() {
        let (address, _accepted) = silent_peer();
        let mut pipeline =
            Pipeline::new(address.clone(), MAX_FRAME, 4).with_io_timeout(Duration::from_millis(40));
        pipeline.send("S", "echo", vec![1]).expect("send");
        let failure = pipeline.recv().expect_err("a silent peer is a failure");
        assert_eq!(failure.code, ErrorCode::Unavailable);
        assert!(
            failure.message.contains("did not answer"),
            "{}",
            failure.message
        );
        assert_eq!(pipeline.in_flight(), 0);
    }

    #[test]
    fn a_zero_deadline_means_no_deadline_rather_than_an_error_from_the_socket() {
        let server = echo_server();
        let client = Client::new(server.local_address().to_string(), MAX_FRAME)
            .with_io_timeout(Duration::ZERO);
        client
            .call("S", "echo", vec![1])
            .expect("the call still works");
    }

    #[test]
    fn a_request_the_peer_could_not_read_is_permanent_rather_than_retryable() {
        // `unavailable` here made a forwarder retry an undecodable batch for a
        // day instead of setting it aside.
        let server = serve(
            "127.0.0.1:0",
            Arc::new(|_: &RpcRequest| malformed("the payload was not a batch"))
                as Arc<dyn Dispatcher>,
            MAX_FRAME,
        )
        .expect("serve");
        let client = Client::new(server.local_address().to_string(), MAX_FRAME);
        let failure = client.call("S", "echo", vec![1]).expect_err("refused");
        assert_eq!(failure.code, ErrorCode::InvalidArgument);
        assert!(!failure.retryable);
    }

    #[test]
    fn a_frame_too_large_to_send_is_permanent_rather_than_retryable() {
        let server = echo_server();
        let client = Client::new(server.local_address().to_string(), 1024);
        let failure = client
            .call("S", "echo", vec![0u8; 4096])
            .expect_err("the frame does not fit");
        assert_eq!(failure.code, ErrorCode::InvalidArgument);
        assert!(!failure.retryable);
    }

    #[test]
    fn a_handler_that_panics_answers_its_caller_and_gives_its_permit_back() {
        // One permit, so a leaked one stops the connection at the next request.
        let told: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&told);
        let server = serve_with(
            "127.0.0.1:0",
            Arc::new(|request: &RpcRequest| match request.op.as_str() {
                "break" => panic!("index out of bounds"),
                _ => reply("EchoResponse", request.payload.clone()),
            }) as Arc<dyn Dispatcher>,
            ServerOptions::new(MAX_FRAME)
                .max_in_flight(1)
                .on_panic(Arc::new(move |service, op, said| {
                    recorder
                        .lock()
                        .unwrap()
                        .push(format!("{service}/{op}: {said}"));
                })),
        )
        .expect("serve");

        let mut pipeline = Pipeline::new(server.local_address().to_string(), MAX_FRAME, 2);
        let broken = pipeline.send("S", "break", vec![1]).expect("send");
        let (id, response) = pipeline
            .recv()
            .expect("recv")
            .expect("a reply, not a silence");
        assert_eq!(id, broken);
        assert_eq!(response.status, Status::Internal);

        let fine = pipeline.send("S", "echo", vec![2]).expect("send");
        let (id, response) = pipeline
            .recv()
            .expect("recv")
            .expect("the connection still serves");
        assert_eq!(id, fine);
        assert_eq!(response.payload, vec![2]);

        assert_eq!(server.stats().handler_panics(), 1);
        assert_eq!(
            *told.lock().unwrap(),
            vec!["S/break: index out of bounds".to_string()]
        );
    }

    #[test]
    fn a_handler_that_panics_on_the_uncorrelated_path_still_answers() {
        let server = serve(
            "127.0.0.1:0",
            Arc::new(|_: &RpcRequest| -> HandlerOutcome { panic!("no") }) as Arc<dyn Dispatcher>,
            MAX_FRAME,
        )
        .expect("serve");
        let stream = TcpStream::connect(server.local_address()).expect("connect");
        let mut carrier = StreamCarrier::with_max_frame(stream, MAX_FRAME).expect("carrier");
        let request = RpcRequest::new("S", "echo", vec![1]);
        carrier
            .send_frame(&request.encode().unwrap())
            .expect("send");
        let frame = carrier.recv_frame().expect("recv").expect("a reply");
        let response = RpcResponse::decode(&frame).expect("decode");
        assert_eq!(response.status, Status::Internal);
    }

    #[test]
    fn a_listener_at_its_connection_limit_refuses_the_next_one_and_counts_it() {
        let server = serve_with(
            "127.0.0.1:0",
            Arc::new(|r: &RpcRequest| reply("EchoResponse", r.payload.clone()))
                as Arc<dyn Dispatcher>,
            ServerOptions::new(MAX_FRAME).max_connections(1),
        )
        .expect("serve");
        let address = server.local_address().to_string();

        let first = Client::new(address.clone(), MAX_FRAME);
        first
            .call("S", "echo", vec![1])
            .expect("the first connection is served");

        let second = Client::new(address.clone(), MAX_FRAME);
        let failure = second
            .call("S", "echo", vec![2])
            .expect_err("the listener is full");
        assert!(failure.retryable, "a full listener empties again");
        assert!(server.stats().refused_connections() >= 1);

        // The place comes back when the first connection goes.
        first.disconnect();
        let mut served = false;
        for _ in 0..200 {
            if server.stats().open_connections() == 0 {
                served = second.call("S", "echo", vec![3]).is_ok();
                break;
            }
            std::thread::yield_now();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(served, "the freed place was never given out");
    }

    #[test]
    fn a_connection_that_sends_nothing_is_closed_at_the_idle_deadline() {
        let server = serve_with(
            "127.0.0.1:0",
            Arc::new(|r: &RpcRequest| reply("EchoResponse", r.payload.clone()))
                as Arc<dyn Dispatcher>,
            ServerOptions::new(MAX_FRAME).idle_timeout(Duration::from_millis(40)),
        )
        .expect("serve");
        let mut stream = TcpStream::connect(server.local_address()).expect("connect");
        // No deadline of our own: this returns only because the server closed.
        let mut buffer = [0u8; 1];
        assert_eq!(stream.read(&mut buffer).expect("a clean close"), 0);
        assert_eq!(server.stats().idle_closed(), 1);
    }

    #[test]
    fn the_idle_deadline_does_not_close_a_connection_that_is_waiting_for_a_reply() {
        let server = serve_with(
            "127.0.0.1:0",
            Arc::new(|r: &RpcRequest| {
                // Three idle periods of work, which is a slow durable write.
                std::thread::sleep(Duration::from_millis(120));
                reply("EchoResponse", r.payload.clone())
            }) as Arc<dyn Dispatcher>,
            ServerOptions::new(MAX_FRAME).idle_timeout(Duration::from_millis(40)),
        )
        .expect("serve");
        let mut pipeline = Pipeline::new(server.local_address().to_string(), MAX_FRAME, 2);
        pipeline.send("S", "echo", vec![7]).expect("send");
        let (_, response) = pipeline
            .recv()
            .expect("recv")
            .expect("the reply still arrives");
        assert_eq!(response.payload, vec![7]);
        assert_eq!(server.stats().idle_closed(), 0);
    }

    #[test]
    fn a_stopped_listener_reports_quiet_once_its_requests_are_answered() {
        let server = echo_server();
        let client = Client::new(server.local_address().to_string(), MAX_FRAME);
        client.call("S", "echo", vec![1]).expect("call");
        server.stop();
        assert!(server.wait_until_quiet(Duration::from_secs(5)));
    }
}
