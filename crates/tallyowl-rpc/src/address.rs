//! Where a connection goes: a TCP address or a unix socket.
//!
//! D62 permits plaintext only where the operator already trusts the host: a
//! loopback TCP address or a unix socket. [`Address::plaintext_permitted`] is
//! that rule in one place, so a service and its configuration check cannot
//! disagree about it.
//!
//! # Unix sockets
//!
//! The file permissions of a socket are its access control, so a listener
//! makes its socket readable and writable by its own user only. A socket file
//! that is already at the path is removed only when nothing answers on it. A
//! live one means another process serves there, and binding over it would take
//! that process's traffic without a word.
//!
//! Windows 10 version 1803 and later has unix sockets, but the Rust standard
//! library supplies them only on Unix. This release builds them on Unix only.
//! On another platform a `unix:` address is refused with a message that says so.

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tallyowl_obs::error::{ErrorCode, TallyOwlError};

/// The prefix that marks a unix socket address.
pub const UNIX_PREFIX: &str = "unix:";

/// One address a service listens on or a client connects to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    /// `host:port`.
    Tcp(String),
    /// `unix:<path>`.
    Unix(PathBuf),
}

impl Address {
    /// Read `host:port` or `unix:<path>`.
    pub fn parse(text: &str) -> Result<Address, TallyOwlError> {
        let text = text.trim();
        if let Some(path) = text.strip_prefix(UNIX_PREFIX) {
            if path.is_empty() {
                return Err(TallyOwlError::new(
                    ErrorCode::InvalidArgument,
                    "The address `unix:` names no file. Write the path of the socket after `unix:`, for example `unix:/run/tallyowl/intake.sock`.",
                ));
            }
            return Ok(Address::Unix(PathBuf::from(path)));
        }
        let Some((host, port)) = text.rsplit_once(':') else {
            return Err(TallyOwlError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "The address `{text}` has no port. Write `host:port`, for example `127.0.0.1:5100`, or `unix:<path>` for a unix socket."
                ),
            ));
        };
        if host.is_empty() || port.parse::<u16>().is_err() {
            return Err(TallyOwlError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "The address `{text}` could not be read. Write `host:port`, for example `127.0.0.1:5100`, or `unix:<path>` for a unix socket."
                ),
            ));
        }
        Ok(Address::Tcp(text.to_string()))
    }

    /// Whether plaintext is permitted here: a loopback TCP address or a unix
    /// socket. See D62.
    ///
    /// A host name other than `localhost` is not loopback, even when it
    /// resolves to a loopback address today. The rule reads what the operator
    /// wrote, so the answer cannot change when DNS does.
    pub fn plaintext_permitted(&self) -> bool {
        match self {
            Address::Unix(_) => true,
            Address::Tcp(text) => {
                let host = text
                    .rsplit_once(':')
                    .map(|(host, _)| host)
                    .unwrap_or(text)
                    .trim_start_matches('[')
                    .trim_end_matches(']');
                if host.eq_ignore_ascii_case("localhost") {
                    return true;
                }
                host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
            }
        }
    }

    /// The host part, for a TLS server name when the caller gives none.
    pub fn host(&self) -> Option<&str> {
        match self {
            Address::Unix(_) => None,
            Address::Tcp(text) => text
                .rsplit_once(':')
                .map(|(host, _)| host.trim_start_matches('[').trim_end_matches(']')),
        }
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Address::Tcp(text) => f.write_str(text),
            Address::Unix(path) => write!(f, "{UNIX_PREFIX}{}", path.display()),
        }
    }
}

#[cfg(not(unix))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "This build of TallyOwl has no unix sockets on this platform. Use a loopback TCP address instead.",
    )
}

/// One connected socket, TCP or unix.
pub enum Socket {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

impl Socket {
    /// A second handle on the same socket.
    pub fn try_clone(&self) -> io::Result<Socket> {
        match self {
            Socket::Tcp(stream) => stream.try_clone().map(Socket::Tcp),
            #[cfg(unix)]
            Socket::Unix(stream) => stream.try_clone().map(Socket::Unix),
        }
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Socket::Tcp(stream) => stream.set_read_timeout(timeout),
            #[cfg(unix)]
            Socket::Unix(stream) => stream.set_read_timeout(timeout),
        }
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Socket::Tcp(stream) => stream.set_write_timeout(timeout),
            #[cfg(unix)]
            Socket::Unix(stream) => stream.set_write_timeout(timeout),
        }
    }

    /// Send small frames at once. A unix socket has no delay to turn off.
    pub(crate) fn set_nodelay(&self) {
        if let Socket::Tcp(stream) = self {
            stream.set_nodelay(true).ok();
        }
    }
}

impl From<TcpStream> for Socket {
    fn from(stream: TcpStream) -> Socket {
        Socket::Tcp(stream)
    }
}

impl Read for Socket {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Socket::Tcp(stream) => stream.read(buffer),
            #[cfg(unix)]
            Socket::Unix(stream) => stream.read(buffer),
        }
    }
}

impl Write for Socket {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Socket::Tcp(stream) => stream.write(buffer),
            #[cfg(unix)]
            Socket::Unix(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Socket::Tcp(stream) => stream.flush(),
            #[cfg(unix)]
            Socket::Unix(stream) => stream.flush(),
        }
    }
}

/// A bound listener, TCP or unix.
pub(crate) enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener),
}

impl Listener {
    /// Bind `address`. A unix socket gets mode 0600, and a stale socket file is
    /// removed first. See the module note.
    pub(crate) fn bind(address: &Address) -> io::Result<Listener> {
        match address {
            Address::Tcp(text) => TcpListener::bind(text.as_str()).map(Listener::Tcp),
            Address::Unix(path) => bind_unix(path),
        }
    }

    /// The TCP address bound. A unix socket has none, and reports the
    /// unspecified address; [`crate::Server::bound`] names its path.
    pub(crate) fn local_address(&self) -> io::Result<SocketAddr> {
        match self {
            Listener::Tcp(listener) => listener.local_addr(),
            #[cfg(unix)]
            Listener::Unix(_) => Ok(SocketAddr::from(([0, 0, 0, 0], 0))),
        }
    }

    /// The address a client uses to reach this listener. Port 0 becomes the
    /// port the operating system chose.
    pub(crate) fn reachable(&self, bound: &Address) -> io::Result<Address> {
        match (self, bound) {
            (Listener::Tcp(listener), Address::Tcp(_)) => {
                Ok(Address::Tcp(listener.local_addr()?.to_string()))
            }
            _ => Ok(bound.clone()),
        }
    }

    /// Wait for the next connection.
    pub(crate) fn accept(&self) -> io::Result<Socket> {
        match self {
            Listener::Tcp(listener) => listener.accept().map(|(stream, _)| Socket::Tcp(stream)),
            #[cfg(unix)]
            Listener::Unix(listener) => listener.accept().map(|(stream, _)| Socket::Unix(stream)),
        }
    }
}

#[cfg(unix)]
fn bind_unix(path: &Path) -> io::Result<Listener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};

    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} exists and is not a socket. Remove it, or choose another path for the socket.",
                    path.display()
                ),
            ));
        }
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "Another process is serving on {}. Stop it, or choose another path for the socket.",
                    path.display()
                ),
            ));
        }
        // Nothing answers: a process that stopped without removing its socket.
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(Listener::Unix(listener))
}

#[cfg(not(unix))]
fn bind_unix(_path: &Path) -> io::Result<Listener> {
    Err(unsupported())
}

/// Connect to `address`. The connect timeout applies to TCP. A unix socket
/// connects at once or fails at once.
pub(crate) fn connect(
    address: &Address,
    connect_timeout: Duration,
) -> Result<Socket, TallyOwlError> {
    match address {
        Address::Tcp(text) => {
            let target = text
                .to_socket_addrs()
                .map_err(|e| {
                    TallyOwlError::new(
                        ErrorCode::InvalidArgument,
                        format!("The address `{text}` could not be read: {e}"),
                    )
                })?
                .next()
                .ok_or_else(|| {
                    TallyOwlError::new(
                        ErrorCode::InvalidArgument,
                        format!("The address `{text}` names no host."),
                    )
                })?;
            TcpStream::connect_timeout(&target, connect_timeout)
                .map(Socket::Tcp)
                .map_err(|e| TallyOwlError::unavailable(format!("We could not reach {text}. {e}")))
        }
        Address::Unix(path) => connect_unix(path),
    }
}

#[cfg(unix)]
fn connect_unix(path: &Path) -> Result<Socket, TallyOwlError> {
    std::os::unix::net::UnixStream::connect(path)
        .map(Socket::Unix)
        .map_err(|e| {
            TallyOwlError::unavailable(format!(
                "We could not reach {UNIX_PREFIX}{}. {e}",
                path.display()
            ))
        })
}

#[cfg(not(unix))]
fn connect_unix(path: &Path) -> Result<Socket, TallyOwlError> {
    Err(TallyOwlError::new(
        ErrorCode::InvalidArgument,
        format!(
            "{UNIX_PREFIX}{} cannot be used. {}",
            path.display(),
            unsupported()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_tcp_or_a_unix_path_and_nothing_else() {
        assert_eq!(
            Address::parse("127.0.0.1:5100").unwrap(),
            Address::Tcp("127.0.0.1:5100".into())
        );
        assert_eq!(
            Address::parse("unix:/run/t.sock").unwrap(),
            Address::Unix(PathBuf::from("/run/t.sock"))
        );
        for bad in ["unix:", "127.0.0.1", ":5100", "host:port", "host:70000", ""] {
            let refused = Address::parse(bad).expect_err(bad);
            assert!(
                refused.message.contains("host:port") || refused.message.contains("unix:"),
                "the refusal of `{bad}` should say what to write: {}",
                refused.message
            );
        }
    }

    #[test]
    fn plaintext_is_permitted_on_loopback_and_unix_and_nowhere_else() {
        for permitted in [
            "127.0.0.1:1",
            "127.9.9.9:1",
            "[::1]:1",
            "localhost:1",
            "LOCALHOST:1",
            "unix:/tmp/a.sock",
        ] {
            assert!(
                Address::parse(permitted).unwrap().plaintext_permitted(),
                "{permitted}"
            );
        }
        for refused in [
            "0.0.0.0:1",
            "[::]:1",
            "10.0.0.1:1",
            "collector.internal:1",
            "localhost.example.com:1",
        ] {
            assert!(
                !Address::parse(refused).unwrap().plaintext_permitted(),
                "{refused}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_file_is_private_to_its_user() {
        use std::os::unix::fs::PermissionsExt;
        let directory = temp_directory("private");
        let path = directory.join("a.sock");
        let _listener = Listener::bind(&Address::Unix(path.clone())).expect("bind");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_socket_is_replaced_and_a_live_one_is_refused() {
        let directory = temp_directory("stale");
        let path = directory.join("a.sock");
        let address = Address::Unix(path.clone());

        let live = Listener::bind(&address).expect("first bind");
        let refused = Listener::bind(&address)
            .err()
            .expect("a live socket is refused");
        assert!(refused.to_string().contains("Another process"), "{refused}");

        // The listener goes away and leaves its file behind.
        drop(live);
        assert!(path.exists());
        Listener::bind(&address).expect("a stale file is replaced");
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_is_not_a_socket_is_never_removed() {
        let directory = temp_directory("not-a-socket");
        let path = directory.join("a.sock");
        std::fs::write(&path, b"someone's data").unwrap();
        let refused = Listener::bind(&Address::Unix(path.clone()))
            .err()
            .expect("refused");
        assert!(refused.to_string().contains("not a socket"), "{refused}");
        assert_eq!(std::fs::read(&path).unwrap(), b"someone's data");
    }

    /// A directory under the system temporary directory, unique to this test,
    /// and removed when the test ends.
    #[cfg(unix)]
    pub(crate) struct Scratch(PathBuf);

    #[cfg(unix)]
    impl std::ops::Deref for Scratch {
        type Target = PathBuf;
        fn deref(&self) -> &PathBuf {
            &self.0
        }
    }

    #[cfg(unix)]
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    pub(crate) fn temp_directory(name: &str) -> Scratch {
        let directory = std::env::temp_dir().join(format!(
            "tallyowl-rpc-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        Scratch(directory)
    }
}
