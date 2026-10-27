//! TLS transport for `brain-coord/1` (rustls, loopback stage).
//!
//! Wraps the coordinator core in real TLS sockets. The HTTP framing, check
//! order, identity binding and fail-closed rules are unchanged: a decrypted
//! stream is handed to the SAME bounded reader and router the plaintext
//! listener uses, so every rule stays in one place.
//!
//! Stage rules enforced here:
//! * Server certificate/key come from owner-configured paths, opened with
//!   real libc flags (`O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC`), validated on the
//!   OPENED descriptor: regular file, size cap, matching uid, and a private
//!   key must not be group/world accessible. A FIFO cannot hang the open.
//! * The client pins to the owner's CA file ONLY — never system roots,
//!   never a silent plaintext/local fallback — and validates the server
//!   name against the certificate SAN.
//! * No danger verifier, no disabled validation, no trust-all.
//! * No secret bytes in Debug/Display/error strings.
//! * One ABSOLUTE deadline per connection (default 5 s, configurable only
//!   downwards) covers handshake, request and response: every socket read
//!   and write, including rustls' internal `complete_io`, gets only the
//!   time that is left, so a peer trickling bytes cannot extend it.
//! * The server handle owns its connection threads: stop refuses further
//!   dispatch, cancels live sockets and joins every thread.
//! * The listener binds `127.0.0.1` only; remote binding is a later gate.
//! * The client never resolves names: it dials a NUMERIC loopback
//!   [`SocketAddr`] supplied by the trusted host and checks the certificate
//!   against a SEPARATE server name. Arbitrary hostname dialing is refused
//!   (a blocking resolver could outlive the absolute deadline), and no
//!   detached resolver thread is ever spawned.

use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rustls::ServerConfig as RustlsServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConnection, StreamOwned};
use serde_json::Value;

use crate::network::auth::AuthStore;
use crate::network::client::{CallResult, parse_response, read_credential_from};
use crate::network::protocol::{
    ENDPOINT, MAX_BODY_BYTES, MAX_CONNECTIONS, MAX_HEADER_BYTES, NetError, PROTOCOL_VERSION,
    READ_TIMEOUT_SECS,
};
use crate::network::server::Inner;

/// Largest PEM material file we will read (certs/keys are a few KiB).
const MAX_PEM_BYTES: u64 = 64 * 1024;
/// Upper bound of the per-connection deadline; configuration may only lower it.
pub const MAX_DEADLINE: Duration = Duration::from_secs(READ_TIMEOUT_SECS);
/// Lower bound, so a typo cannot make every handshake fail instantly.
pub const MIN_DEADLINE: Duration = Duration::from_millis(50);
/// How often the nonblocking accept loop looks at the stop flag.
const ACCEPT_POLL: Duration = Duration::from_millis(10);
/// Largest response the client will buffer.
const MAX_RESPONSE_BYTES: u64 = (MAX_HEADER_BYTES + 4 * MAX_BODY_BYTES) as u64;

/// Where the TLS server finds its material.
#[derive(Clone, Debug)]
pub struct TlsServerConfig {
    pub state_dir: PathBuf,
    pub port: u16,
    pub cert_file: PathBuf,
    pub key: KeySource,
    /// Absolute per-connection deadline, `MIN_DEADLINE..=MAX_DEADLINE`.
    pub deadline: Duration,
    /// Numeric listen address; `127.0.0.1` by default.
    pub listen: IpAddr,
    /// Affirmative opt-in for a non-loopback `listen` (see
    /// [`checked_listen_ip`](crate::network::server::checked_listen_ip)).
    pub allow_lan: bool,
    /// Affirmative opt-in for a CGNAT/overlay address (`RFC 6598 range`), separate from LAN.
    pub allow_cgnat: bool,
}

/// Where a private key (or credential) comes from: an owner-only file, or
/// a descriptor the trusted parent left open (so no path is ever shown).
/// Both are validated on the OPENED descriptor with the same rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeySource {
    File(PathBuf),
    /// An inherited descriptor number, `>= 3` (never stdin/stdout/stderr).
    Fd(i32),
}

impl TlsServerConfig {
    /// Loopback config with the default (maximum) 5 s deadline.
    pub fn new(
        state_dir: impl Into<PathBuf>,
        port: u16,
        cert_file: impl Into<PathBuf>,
        key_file: impl Into<PathBuf>,
    ) -> Self {
        Self {
            state_dir: state_dir.into(),
            port,
            cert_file: cert_file.into(),
            key: KeySource::File(key_file.into()),
            deadline: MAX_DEADLINE,
            listen: IpAddr::V4(Ipv4Addr::LOCALHOST),
            allow_lan: false,
            allow_cgnat: false,
        }
    }
}

/// Which CA the client pins to (owner-configured, never system roots),
/// where it dials, and which name the certificate must carry.
#[derive(Clone, Debug)]
pub struct TlsClientConfig {
    pub ca_file: PathBuf,
    /// Name checked against the certificate SAN (`localhost` or
    /// `127.0.0.1`) and sent as `Host`. It is NEVER resolved.
    pub server_name: String,
    /// Numeric dial target from the trusted host; loopback only at this stage.
    pub dial: SocketAddr,
    /// Absolute deadline for connect + handshake + request + response.
    pub deadline: Duration,
    /// Affirmative opt-in to dial a private unicast LAN address.
    pub allow_lan: bool,
    /// Affirmative opt-in for a CGNAT/overlay address (`RFC 6598 range`), separate from LAN.
    pub allow_cgnat: bool,
}

impl TlsClientConfig {
    pub fn new(
        ca_file: impl Into<PathBuf>,
        server_name: impl Into<String>,
        dial: SocketAddr,
    ) -> Self {
        Self {
            ca_file: ca_file.into(),
            server_name: server_name.into(),
            dial,
            deadline: MAX_DEADLINE,
            allow_lan: false,
            allow_cgnat: false,
        }
    }
}

/// Parses a dial target WITHOUT any name resolution: only a numeric
/// `IP:PORT` (`[::1]:PORT` for IPv6) on a loopback address with a non-zero
/// port is accepted. Hostnames are refused, never looked up.
///
/// # Errors
/// [`TlsError::InvalidConfig`] for a hostname, a missing/zero port or a
/// non-loopback address (LAN dialing is a separate deployment gate).
pub fn parse_dial_target(text: &str) -> Result<SocketAddr, TlsError> {
    let address: SocketAddr = text.parse().map_err(|_| {
        TlsError::InvalidConfig(
            "dial target must be a numeric IP:PORT; hostnames are never resolved".into(),
        )
    })?;
    checked_dial(address, false, false)
}

/// As [`parse_dial_target`], but with `allow_lan` a numeric private unicast
/// LAN address passes the same policy as a LAN listener. Still never resolves.
///
/// # Errors
/// [`TlsError::InvalidConfig`] as for [`parse_dial_target`], or for a
/// non-LAN address even with the opt-in.
pub fn parse_dial_target_lan(text: &str, allow_lan: bool) -> Result<SocketAddr, TlsError> {
    parse_dial_target_with(text, allow_lan, false)
}

/// As [`parse_dial_target_lan`], with the separate CGNAT/overlay opt-in.
///
/// # Errors
/// As [`parse_dial_target_lan`].
pub fn parse_dial_target_with(
    text: &str,
    allow_lan: bool,
    allow_cgnat: bool,
) -> Result<SocketAddr, TlsError> {
    let address: SocketAddr = text.parse().map_err(|_| {
        TlsError::InvalidConfig(
            "dial target must be a numeric IP:PORT; hostnames are never resolved".into(),
        )
    })?;
    checked_dial(address, allow_lan, allow_cgnat)
}

fn checked_dial(
    address: SocketAddr,
    allow_lan: bool,
    allow_cgnat: bool,
) -> Result<SocketAddr, TlsError> {
    if address.port() == 0 {
        return Err(TlsError::InvalidConfig(
            "dial target needs a non-zero port".into(),
        ));
    }
    if crate::network::server::is_cgnat(address.ip()) {
        if !allow_cgnat {
            return Err(TlsError::InvalidConfig(format!(
                "{} is a shared CGNAT/overlay address; it needs the explicit CGNAT dial opt-in",
                address.ip()
            )));
        }
    } else if !address.ip().is_loopback() {
        if !allow_lan {
            return Err(TlsError::InvalidConfig(
                "only loopback dial targets are allowed without the explicit LAN opt-in".into(),
            ));
        }
        crate::network::server::checked_listen_ip(address.ip(), true).map_err(|_| {
            TlsError::InvalidConfig(format!(
                "{} is not a private unicast LAN dial target",
                address.ip()
            ))
        })?;
    }
    Ok(address)
}

/// A byte transport whose I/O remains bounded by the caller deadline.
pub trait Transport: Read + Write + Send {
    fn set_timeout(&self, timeout: Duration) -> io::Result<()>;
}
impl Transport for TcpStream {
    fn set_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(timeout))?;
        self.set_write_timeout(Some(timeout))
    }
}

/// Opens a numeric direct socket or an explicitly configured relay tunnel.
/// The caller passes only the time left before its absolute deadline.
pub trait Dialer: Send + Sync {
    /// Connects to `address` within `timeout`.
    ///
    /// # Errors
    /// Any connect error; the caller reports it as unreachable.
    fn dial(&self, address: SocketAddr, timeout: Duration) -> io::Result<TcpStream>;

    /// Open a transport; relay dialers return an outer TLS tunnel here.
    fn open(&self, address: SocketAddr, timeout: Duration) -> io::Result<Box<dyn Transport>> {
        self.dial(address, timeout)
            .map(|s| Box::new(s) as Box<dyn Transport>)
    }
}

/// The production dialer: `connect_timeout` on the numeric address.
pub struct TcpDialer;

impl Dialer for TcpDialer {
    fn dial(&self, address: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
        TcpStream::connect_timeout(&address, timeout)
    }
}

/// TLS transport errors with stable string codes.
#[derive(Clone, PartialEq, Eq)]
pub enum TlsError {
    /// Stable code: `invalid_config`.
    InvalidConfig(String),
    /// Stable code: `tls_failure`.
    TlsFailure(String),
    /// Stable code: `unreachable`.
    Unreachable(String),
}

impl TlsError {
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidConfig(_) => "invalid_config",
            Self::TlsFailure(_) => "tls_failure",
            Self::Unreachable(_) => "unreachable",
        }
    }
}

impl fmt::Debug for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(f, "invalid_config: {message}"),
            Self::TlsFailure(message) => write!(f, "tls_failure: {message}"),
            Self::Unreachable(message) => write!(f, "unreachable: {message}"),
        }
    }
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

fn checked_deadline(deadline: Duration) -> Result<Duration, TlsError> {
    if (MIN_DEADLINE..=MAX_DEADLINE).contains(&deadline) {
        Ok(deadline)
    } else {
        Err(TlsError::InvalidConfig(format!(
            "deadline must be {}..={} ms",
            MIN_DEADLINE.as_millis(),
            MAX_DEADLINE.as_millis()
        )))
    }
}

pub(crate) fn current_uid() -> Option<u32> {
    crate::platform::current_uid().ok()
}

/// Open an inherited descriptor >=3. Linux reopens through /proc with its own
/// O_NONBLOCK flags; Darwin duplicates with F_DUPFD_CLOEXEC and accepts regular
/// files only. The caller revalidates owner, mode and size on the open handle.
/// Darwin never changes flags shared with the parent.
pub fn open_inherited_fd(fd: i32) -> io::Result<std::fs::File> {
    if fd < 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "descriptor must be >= 3",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(format!("/proc/self/fd/{fd}"))
    }
    #[cfg(target_os = "macos")]
    {
        // fdescfs duplicates the open description. Never set O_NONBLOCK on it:
        // reject non-regular duplicates before any read, then validate owner/mode/size.
        use std::os::fd::FromRawFd;
        // SAFETY: fcntl accepts arbitrary integers and reports invalid fds.
        #[allow(unsafe_code)]
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fcntl returned a new owned descriptor.
        #[allow(unsafe_code)]
        let file = unsafe { std::fs::File::from_raw_fd(duplicate) };
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "inherited TLS descriptor must be a regular file",
            ));
        }
        Ok(file)
    }
}

/// Secure-opens one material file and validates the OPENED descriptor.
///
/// `O_NOFOLLOW` refuses symlinks, `O_NONBLOCK` makes a FIFO open return
/// instead of hanging, `O_CLOEXEC` keeps it out of children. Checks run on
/// the descriptor's own metadata: regular file, size cap, uid match;
/// `private_key` additionally requires group/world bits clear.
fn open_material(path: &Path, private_key: bool) -> Result<Vec<u8>, TlsError> {
    open_source(&KeySource::File(path.to_path_buf()), private_key)
}

fn open_source(source: &KeySource, private_key: bool) -> Result<Vec<u8>, TlsError> {
    let kind = if private_key { "key" } else { "certificate" };
    let (file, path) = match source {
        KeySource::File(path) => (
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(path),
            path.clone(),
        ),
        KeySource::Fd(fd) => (open_inherited_fd(*fd), PathBuf::from(format!("fd {fd}"))),
    };
    let path = path.as_path();
    let file = file.map_err(|error| {
        TlsError::InvalidConfig(format!(
            "cannot open {kind} {}: {}",
            path.display(),
            error.kind()
        ))
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| TlsError::InvalidConfig(format!("{kind} metadata unreadable")))?;
    if !metadata.is_file() {
        return Err(TlsError::InvalidConfig(format!(
            "{kind} {} is not a regular file",
            path.display()
        )));
    }
    if metadata.size() > MAX_PEM_BYTES {
        return Err(TlsError::InvalidConfig(format!(
            "{kind} {} exceeds {MAX_PEM_BYTES} bytes",
            path.display()
        )));
    }
    match current_uid() {
        Some(uid) if metadata.uid() == uid => {}
        Some(_) => {
            return Err(TlsError::InvalidConfig(format!(
                "{kind} {} is not owned by this uid",
                path.display()
            )));
        }
        // Fail closed where the owner cannot be established.
        None => {
            return Err(TlsError::InvalidConfig(
                "cannot establish the current uid".into(),
            ));
        }
    }
    if private_key && metadata.permissions().mode() & 0o077 != 0 {
        return Err(TlsError::InvalidConfig(format!(
            "private key {} is group/world accessible",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_PEM_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| TlsError::InvalidConfig(format!("{kind} unreadable")))?;
    if bytes.len() as u64 > MAX_PEM_BYTES {
        return Err(TlsError::InvalidConfig(format!(
            "{kind} exceeds cap while reading"
        )));
    }
    Ok(bytes)
}

/// Loads a certificate chain (all PEM `CERTIFICATE` sections) from bytes.
fn parse_certs(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| TlsError::InvalidConfig("certificate file holds no usable PEM cert".into()))?;
    if certs.is_empty() {
        return Err(TlsError::InvalidConfig(
            "certificate file holds no PEM cert".into(),
        ));
    }
    Ok(certs)
}

/// Loads the first private key from PEM bytes (PKCS#8, PKCS#1 or SEC1).
/// The error never carries key bytes.
fn parse_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>, TlsError> {
    PrivateKeyDer::from_pem_slice(pem)
        .map_err(|_| TlsError::InvalidConfig("key file holds no usable PEM key".into()))
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Builds the rustls server config from owner-configured material paths.
///
/// # Errors
/// [`TlsError::InvalidConfig`] for material/permission problems,
/// [`TlsError::TlsFailure`] when rustls rejects the pair.
pub fn server_config(config: &TlsServerConfig) -> Result<RustlsServerConfig, TlsError> {
    let cert_bytes = open_material(&config.cert_file, false)?;
    let key_bytes = open_source(&config.key, true)?;
    let certs = parse_certs(&cert_bytes)?;
    let key = parse_key(&key_bytes)?;
    RustlsServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|_| TlsError::TlsFailure("no safe protocol versions".into()))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|_| TlsError::TlsFailure("server certificate and key were rejected".into()))
}

/// Builds the rustls client config pinned to the owner's CA file ONLY.
///
/// # Errors
/// [`TlsError::InvalidConfig`] when the CA file is missing/unusable.
pub fn client_config(config: &TlsClientConfig) -> Result<ClientConfig, TlsError> {
    let ca_bytes = open_material(&config.ca_file, false)?;
    client_config_pem(&ca_bytes)
}

fn client_config_pem(ca_bytes: &[u8]) -> Result<ClientConfig, TlsError> {
    let mut roots = RootCertStore::empty();
    for cert in parse_certs(ca_bytes)? {
        roots
            .add(cert)
            .map_err(|_| TlsError::InvalidConfig("CA file holds an unusable certificate".into()))?;
    }
    Ok(ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|_| TlsError::TlsFailure("no safe protocol versions".into()))?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

fn tls_to_net(error: TlsError) -> NetError {
    match error {
        TlsError::InvalidConfig(message) => NetError::invalid(format!("invalid_config: {message}")),
        TlsError::TlsFailure(message) => NetError::unavailable(format!("tls_failure: {message}")),
        TlsError::Unreachable(message) => NetError::unavailable(format!("unreachable: {message}")),
    }
}

/// A socket whose every read and write is bounded by ONE absolute instant.
///
/// Each operation sets the socket timeout to the time that is LEFT, so
/// bytes arriving slowly never extend the total; once the instant passes
/// every operation fails with `TimedOut`. rustls drives all its I/O
/// (handshake included) through this wrapper.
struct DeadlineStream {
    sock: Box<dyn Transport>,
    deadline: Instant,
}

impl DeadlineStream {
    fn remaining(&self) -> io::Result<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection deadline exceeded",
            ))
        } else {
            Ok(left)
        }
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.remaining()?;
        self.sock.set_timeout(left)?;
        self.sock.read(buf)
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let left = self.remaining()?;
        self.sock.set_timeout(left)?;
        self.sock.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

/// Drives a handshake to completion under the stream's deadline.
fn complete_handshake<C, S>(tls: &mut StreamOwned<C, DeadlineStream>) -> io::Result<()>
where
    C: std::ops::DerefMut<Target = rustls::ConnectionCommon<S>>,
    S: rustls::SideData,
{
    while tls.conn.is_handshaking() {
        tls.sock.remaining()?;
        tls.conn.complete_io(&mut tls.sock)?;
    }
    Ok(())
}

/// Sends close_notify and drains pending TLS records, all under the deadline.
fn close_tls<C, S>(tls: &mut StreamOwned<C, DeadlineStream>)
where
    C: std::ops::DerefMut<Target = rustls::ConnectionCommon<S>>,
    S: rustls::SideData,
{
    tls.conn.send_close_notify();
    while tls.conn.wants_write() {
        if tls.conn.write_tls(&mut tls.sock).is_err() {
            break;
        }
    }
    let _ = tls.sock.flush();
}

/// A TLS server bound to loopback, serving the same protocol as
/// [`crate::network::CoordServer`] over an encrypted wire.
pub struct TlsCoordServer {
    listener: TcpListener,
    port: u16,
    inner: Arc<TlsInner>,
}

struct TlsInner {
    tls: Arc<RustlsServerConfig>,
    core: Inner,
    deadline: Duration,
    /// Live connection sockets (clones), so stop can cancel them.
    conns: Mutex<Vec<(u64, TcpStream)>>,
    next_id: AtomicU64,
}

impl TlsInner {
    fn register(&self, stream: &TcpStream) -> Option<u64> {
        let clone = stream.try_clone().ok()?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.conns.lock().ok()?.push((id, clone));
        Some(id)
    }

    fn unregister(&self, id: u64) {
        if let Ok(mut conns) = self.conns.lock() {
            conns.retain(|(conn_id, _)| *conn_id != id);
        }
    }

    fn live(&self) -> usize {
        self.conns.lock().map(|conns| conns.len()).unwrap_or(0)
    }

    /// Shuts every live socket down: blocked reads/writes return at once.
    fn cancel_all(&self) {
        if let Ok(conns) = self.conns.lock() {
            for (_, stream) in conns.iter() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }
}

impl TlsCoordServer {
    /// Opens the stores and binds `127.0.0.1:port` (port 0 = ephemeral).
    /// Nothing is served until [`TlsCoordServer::serve`].
    ///
    /// # Errors
    /// [`NetError`] for a bad deadline, unusable material, a store that
    /// cannot open, or a bind failure. No plaintext listener is opened.
    pub fn start(config: &TlsServerConfig) -> Result<Self, NetError> {
        // The binding policy is checked first: a refused address loads no
        // key, opens no store and binds nothing.
        let listen = crate::network::server::checked_listen_ip_with(
            config.listen,
            config.allow_lan,
            config.allow_cgnat,
        )?;
        let deadline = checked_deadline(config.deadline).map_err(tls_to_net)?;
        let tls = server_config(config).map_err(tls_to_net)?;
        // `Host` must also accept the exact bound IP, so a client whose
        // server name is that IP (the certificate's IP SAN) is not refused.
        let leaf = parse_certs(&open_material(&config.cert_file, false).map_err(tls_to_net)?)
            .map_err(tls_to_net)?
            .into_iter()
            .next()
            .ok_or_else(|| NetError::invalid("server certificate file holds no certificate"))?;
        let core = Inner::open(&config.state_dir)?
            .with_bound_host(listen)
            .with_certificate(leaf);
        let address = SocketAddr::new(listen, config.port);
        let listener = TcpListener::bind(address)
            .map_err(|error| NetError::unavailable(format!("bind {address}: {error}")))?;
        listener
            .set_nonblocking(true)
            .map_err(|_| NetError::unavailable("cannot make the listener nonblocking"))?;
        let port = listener
            .local_addr()
            .map(|addr| addr.port())
            .unwrap_or(config.port);
        Ok(Self {
            listener,
            port,
            inner: Arc::new(TlsInner {
                tls: Arc::new(tls),
                core,
                deadline,
                conns: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(0),
            }),
        })
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Serves on an owned background thread until the handle stops.
    ///
    /// # Errors
    /// `NetError::Unavailable` when the accept thread cannot be spawned.
    pub fn serve(self) -> Result<TlsServerHandle, NetError> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let inner = Arc::clone(&self.inner);
        let listener = self.listener;
        let join = thread::Builder::new()
            .name("brain-coord-tls-accept".to_owned())
            .spawn(move || accept_loop(&listener, &inner, &flag))
            .map_err(|_| NetError::unavailable("cannot spawn the accept thread"))?;
        Ok(TlsServerHandle {
            shutdown,
            join: Some(join),
            port: self.port,
            inner: self.inner,
        })
    }
}

/// Accepts until the flag is set; returns the owned connection threads.
fn accept_loop(
    listener: &TcpListener,
    inner: &Arc<TlsInner>,
    flag: &AtomicBool,
) -> Vec<thread::JoinHandle<()>> {
    let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();
    while !flag.load(Ordering::SeqCst) {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(_) => {
                thread::sleep(ACCEPT_POLL);
                continue;
            }
        };
        workers.retain(|worker| !worker.is_finished());
        if workers.len() >= MAX_CONNECTIONS || stream.set_nonblocking(false).is_err() {
            drop(stream);
            continue;
        }
        let Some(id) = inner.register(&stream) else {
            continue;
        };
        let spawned = thread::Builder::new()
            .name("brain-coord-tls-conn".to_owned())
            .spawn({
                let inner = Arc::clone(inner);
                move || {
                    handle_tls_connection(stream, &inner);
                    inner.unregister(id);
                }
            });
        match spawned {
            Ok(worker) => workers.push(worker),
            Err(_) => inner.unregister(id),
        }
    }
    workers
}

/// Owns the accept thread and every connection thread it started.
/// [`TlsServerHandle::stop`] (or drop) refuses further dispatch, cancels
/// live sockets and joins everything; no thread outlives the handle.
pub struct TlsServerHandle {
    shutdown: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<Vec<thread::JoinHandle<()>>>>,
    port: u16,
    inner: Arc<TlsInner>,
}

impl TlsServerHandle {
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Connections currently being served.
    #[must_use]
    pub fn live_connections(&self) -> usize {
        self.inner.live()
    }

    /// Runs one local administration function against the live auth
    /// store under the state lock.
    ///
    /// # Errors
    /// Whatever `admin` returns, or `NetError::Unavailable` on poison.
    pub fn auth_admin<T>(
        &self,
        admin: impl FnOnce(&AuthStore) -> Result<T, NetError>,
    ) -> Result<T, NetError> {
        self.inner.core.auth_admin(admin)
    }

    /// Stops serving and returns how many connection threads were joined.
    pub fn stop(mut self) -> usize {
        self.stop_now()
    }

    fn stop_now(&mut self) -> usize {
        // Order matters: refuse dispatch first, then stop accepting, then
        // cancel the sockets the (now finished) accept loop registered.
        self.inner.core.stop();
        self.shutdown.store(true, Ordering::SeqCst);
        let workers = match self.join.take() {
            Some(join) => join.join().unwrap_or_default(),
            None => return 0,
        };
        self.inner.cancel_all();
        let mut joined = 0;
        for worker in workers {
            if worker.join().is_ok() {
                joined += 1;
            }
        }
        joined
    }
}

impl Drop for TlsServerHandle {
    fn drop(&mut self) {
        self.stop_now();
    }
}

/// Serves one TLS connection: handshake, then one request/response through
/// the core's bounded reader and router — all under one absolute deadline.
fn handle_tls_connection(stream: TcpStream, inner: &TlsInner) {
    let deadline = Instant::now() + inner.deadline;
    let Ok(conn) = ServerConnection::new(Arc::clone(&inner.tls)) else {
        return;
    };
    let mut tls = StreamOwned::new(
        conn,
        DeadlineStream {
            sock: Box::new(stream),
            deadline,
        },
    );
    if complete_handshake(&mut tls).is_err() {
        return; // failed or too slow: drop, nothing is dispatched
    }
    inner.core.serve_stream(&mut tls);
    close_tls(&mut tls);
}

/// TLS client for `brain-coord/1`: pinned CA roots only, no fallback.
pub struct TlsCoordClient {
    config: Arc<ClientConfig>,
    server_name: ServerName<'static>,
    host: String,
    dial: SocketAddr,
    deadline: Duration,
    token: String,
    dialer: Arc<dyn Dialer>,
}

impl fmt::Debug for TlsCoordClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsCoordClient")
            .field("server_name", &self.host)
            .field("dial", &self.dial)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Outcome of one TLS call.
pub enum TlsCallResult {
    /// HTTP 200 with the full protocol envelope.
    Ok(Value),
    /// The server answered with a transport refusal (401/403/404/413...).
    Refused(NetError),
    /// No TCP connection or TLS session got established (refused connect,
    /// wrong CA, wrong name, expired cert, plaintext peer, deadline before
    /// the handshake finished): nothing was sent, nothing was dispatched.
    Tls(TlsError),
    /// Connection lost or deadline passed. After the request was written
    /// the mutation state is UNKNOWN — retry the same `request_id`.
    Unreachable(String),
}

impl fmt::Debug for TlsCallResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok(_) => f.write_str("TlsCallResult::Ok(<envelope>)"),
            Self::Refused(error) => write!(f, "TlsCallResult::Refused({})", error.status()),
            Self::Tls(error) => write!(f, "TlsCallResult::Tls({error})"),
            Self::Unreachable(message) => write!(f, "TlsCallResult::Unreachable({message})"),
        }
    }
}

impl TlsCoordClient {
    /// Pins to the owner's CA file and loads the credential with the same
    /// secure rules as the plaintext client (FD-based, no symlink, 0600).
    ///
    /// # Errors
    /// [`NetError::InvalidRequest`] for a bad deadline, credential or CA.
    pub fn new(tls: TlsClientConfig, credential_file: &Path) -> Result<Self, NetError> {
        Self::with_dialer(
            tls,
            &KeySource::File(credential_file.to_path_buf()),
            Arc::new(TcpDialer),
        )
    }

    /// As [`TlsCoordClient::new`] with an explicit [`Dialer`].
    ///
    /// # Errors
    /// As [`TlsCoordClient::new`].
    pub fn with_dialer(
        tls: TlsClientConfig,
        credential: &KeySource,
        dialer: Arc<dyn Dialer>,
    ) -> Result<Self, NetError> {
        let deadline = checked_deadline(tls.deadline).map_err(tls_to_net)?;
        let dial = checked_dial(tls.dial, tls.allow_lan, tls.allow_cgnat).map_err(tls_to_net)?;
        let server_name = ServerName::try_from(tls.server_name.clone())
            .map_err(|_| NetError::invalid("server name is not a valid DNS name or IP"))?;
        let config = client_config(&tls).map_err(tls_to_net)?;
        let token = read_credential_from(credential)?;
        Ok(Self {
            config: Arc::new(config),
            server_name,
            host: tls.server_name,
            dial,
            deadline,
            token,
            dialer,
        })
    }

    /// A host-local team source, enrolled over pinned TLS. Never logs its secret.
    pub(crate) fn team_source(source: &super::team::Source) -> Result<Self, NetError> {
        let dial = parse_dial_target_with(&source.dial, source.allow_lan, source.allow_cgnat)
            .map_err(tls_to_net)?;
        let token = super::BearerToken::parse(&source.credential)?;
        let config = client_config_pem(source.ca_pem.as_bytes()).map_err(tls_to_net)?;
        Ok(Self {
            config: Arc::new(config),
            server_name: ServerName::try_from(source.server_name.clone())
                .map_err(|_| NetError::invalid("invalid source server name"))?,
            host: source.server_name.clone(),
            dial,
            deadline: Duration::from_millis(3000),
            token: token.as_str().into(),
            dialer: Arc::new(TcpDialer),
        })
    }

    /// The numeric address this client dials.
    #[must_use]
    pub fn dial_target(&self) -> SocketAddr {
        self.dial
    }

    /// The name the server certificate must carry.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.host
    }

    /// One allowlisted tool call over TLS, bounded by the client deadline.
    pub fn call(
        &self,
        tool: &str,
        agent: &str,
        arguments: serde_json::Map<String, Value>,
        request_id: Option<String>,
    ) -> TlsCallResult {
        let deadline = Instant::now() + self.deadline;
        let mut body = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "tool": tool,
            "arguments": arguments,
        });
        // An empty agent is omitted: the server derives it from the credential.
        if !agent.is_empty() {
            body["agent"] = serde_json::json!(agent);
        }
        if let Some(id) = &request_id {
            body["request_id"] = serde_json::json!(id);
        }
        let Ok(payload) = serde_json::to_vec(&body) else {
            return TlsCallResult::Refused(NetError::invalid("request is not encodable"));
        };
        if payload.len() > MAX_BODY_BYTES {
            return TlsCallResult::Refused(NetError::TooLarge("request too large".into()));
        }
        let sock = match connect_before(self.dialer.as_ref(), self.dial, deadline) {
            Ok(sock) => sock,
            // Nothing was sent: safe to retry, never ambiguous.
            Err(message) => return TlsCallResult::Tls(TlsError::Unreachable(message)),
        };
        let Ok(conn) = ClientConnection::new(Arc::clone(&self.config), self.server_name.clone())
        else {
            return TlsCallResult::Tls(TlsError::TlsFailure("client setup failed".into()));
        };
        let mut tls = StreamOwned::new(conn, DeadlineStream { sock, deadline });
        if let Err(error) = complete_handshake(&mut tls) {
            // rustls reports certificate/name/protocol failures as InvalidData.
            return if error.kind() == io::ErrorKind::InvalidData {
                TlsCallResult::Tls(TlsError::TlsFailure(
                    "handshake refused (untrusted CA, wrong name, expired or not TLS)".into(),
                ))
            } else {
                TlsCallResult::Tls(TlsError::Unreachable(format!(
                    "handshake did not finish: {}",
                    error.kind()
                )))
            };
        }
        // The token goes into the Authorization header only.
        let head = format!(
            "POST {ENDPOINT} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.host,
            self.token,
            payload.len()
        );
        if tls.write_all(head.as_bytes()).is_err()
            || tls.write_all(&payload).is_err()
            || tls.flush().is_err()
        {
            return TlsCallResult::Unreachable(
                "write failed (state unknown; retry the same request_id)".into(),
            );
        }
        let mut response = Vec::new();
        if (&mut tls)
            .take(MAX_RESPONSE_BYTES)
            .read_to_end(&mut response)
            .is_err()
        {
            return TlsCallResult::Unreachable(
                "read failed (state unknown; retry the same request_id)".into(),
            );
        }
        close_tls(&mut tls);
        match parse_response(&response) {
            CallResult::Ok(body) => TlsCallResult::Ok(body),
            CallResult::Refused(error) => TlsCallResult::Refused(error),
            CallResult::Unreachable(message) => TlsCallResult::Unreachable(message),
        }
    }
}

/// Connects to the numeric `address` with only the time left before
/// `deadline`. No name is involved, so no resolver can block here; a
/// dialer that overruns is still caught by the deadline check after it.
fn connect_before(
    dialer: &dyn Dialer,
    address: SocketAddr,
    deadline: Instant,
) -> Result<Box<dyn Transport>, String> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err("connect deadline exceeded".into());
    }
    let stream = dialer
        .open(address, left)
        .map_err(|error| format!("connect: {}", error.kind()))?;
    if Instant::now() >= deadline {
        return Err("connect deadline exceeded".into());
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_inherited_fd_is_regular_and_bad_descriptors_are_refused() {
        use std::os::fd::AsRawFd;
        let dir = crate::test_support::temp_path("darwin-tls-fd");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("synthetic");
        std::fs::write(&path, b"synthetic").unwrap();
        let file = std::fs::File::open(path).unwrap();
        assert!(
            super::open_inherited_fd(file.as_raw_fd())
                .unwrap()
                .metadata()
                .unwrap()
                .is_file()
        );
        assert!(super::open_inherited_fd(-1).is_err());
        assert!(super::open_inherited_fd(i32::MAX).is_err());
        let (socket, _other) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(super::open_inherited_fd(socket.as_raw_fd()).is_err());
    }

    /// G12: dial targets follow the policy table — loopback always, LAN only
    /// with the explicit opt-in, CGNAT (100.64/10) only with its own opt-in,
    /// hostnames never. Boundaries of the CGNAT range are pinned.
    #[test]
    fn dial_targets_follow_the_policy_table() {
        let cases: &[(&str, bool, bool, bool)] = &[
            ("127.0.0.1:7443", false, false, true),
            ("[::1]:7443", false, false, true),
            ("127.0.0.1:0", false, false, false),
            ("10.0.0.5:7443", false, false, false),
            ("10.0.0.5:7443", true, false, true),
            ("192.168.1.10:7443", true, false, true),
            ("169.254.9.9:7443", true, false, false),
            ("224.0.0.1:7443", true, false, false),
            ("8.8.8.8:53", true, false, false),
            ("172.16.5.5:7443", true, false, true),
            ("[fc00::1]:7443", false, false, false),
            ("[fc00::1]:7443", true, false, true),
            ("[fe80::1%3]:7443", true, false, false),
            ("[::ffff:10.0.0.5]:7443", true, false, false),
            // 100.63/100.128 are outside CGNAT and outside RFC1918: refused
            // in every mode (the boundary is exclusive on both sides).
            ("100.63.255.255:7443", true, false, false),
            ("100.63.255.255:7443", false, true, false),
            ("100.63.255.255:7443", true, true, false),
            ("100.64.0.0:7443", false, false, false),
            ("100.64.0.0:7443", true, false, false),
            ("100.64.0.0:7443", false, true, true),
            ("100.127.255.255:7443", false, true, true),
            ("100.128.0.0:7443", false, true, false),
            ("100.128.0.0:7443", true, true, false),
            ("[fd7a::1]:7443", false, false, false),
            ("[fd7a::1]:7443", true, false, true),
            ("[fe80::1%3]:7443", true, false, false),
            ("[fe80::1%eth0]:7443", true, false, false),
            ("0.0.0.0:7443", true, false, false),
            ("example.com:7443", true, true, false),
            ("10.0.0.5", false, false, false),
            ("10.0.0.5:99999", false, false, false),
            ("10.0.0.5:-1", false, false, false),
            ("", false, false, false),
        ];
        for (input, allow_lan, allow_cgnat, expected) in cases {
            let parsed = parse_dial_target_with(input, *allow_lan, *allow_cgnat);
            assert_eq!(
                parsed.is_ok(),
                *expected,
                "{input} lan={allow_lan} cgnat={allow_cgnat}: {parsed:?}"
            );
        }
    }
    use super::*;
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use std::sync::OnceLock;

    /// Test-only CA, server, wrong-host, expired and other-CA material,
    /// generated once per test process by the local `openssl` binary with a
    /// fixed argv (no shell). Nothing is checked in, nothing expires under
    /// the tests' feet, and none of it is ever used outside tests. A missing
    /// `openssl` FAILS the tests; it never silently skips them.
    fn material() -> &'static Path {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        DIR.get_or_init(|| {
            // Process-lifetime data shared by every TLS test thread: NOT
            // registered in the thread-local cleanup, whose Drop would race
            // with other threads still reading the material.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir()
                .join(format!("brain-tls-material-{}-{nanos}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("material dir");
            let ca = |name: &str, cn: &str| {
                openssl(
                    &dir,
                    &[
                        "req",
                        "-x509",
                        "-newkey",
                        "ec",
                        "-pkeyopt",
                        "ec_paramgen_curve:P-256",
                        "-nodes",
                        "-keyout",
                        &format!("{name}.key"),
                        "-out",
                        &format!("{name}.crt"),
                        "-subj",
                        &format!("/CN={cn}"),
                        "-days",
                        "2",
                        "-addext",
                        "basicConstraints=critical,CA:TRUE",
                        "-addext",
                        "keyUsage=critical,keyCertSign",
                    ],
                );
            };
            ca("ca", "brain-coord-test-ca");
            ca("other-ca", "brain-coord-other-ca");
            let leaf = |name: &str, san: &str, validity: &[&str]| {
                openssl(
                    &dir,
                    &[
                        "req",
                        "-newkey",
                        "ec",
                        "-pkeyopt",
                        "ec_paramgen_curve:P-256",
                        "-nodes",
                        "-keyout",
                        &format!("{name}.key"),
                        "-out",
                        &format!("{name}.csr"),
                        "-subj",
                        "/CN=brain-coord-test-leaf",
                    ],
                );
                std::fs::write(
                    dir.join(format!("{name}.ext")),
                    format!("subjectAltName={san}\nbasicConstraints=CA:FALSE\n"),
                )
                .expect("ext");
                let mut args = vec![
                    "x509",
                    "-req",
                    "-in",
                    &*format!("{name}.csr"),
                    "-CA",
                    "ca.crt",
                    "-CAkey",
                    "ca.key",
                    "-out",
                    &*format!("{name}.crt"),
                    "-extfile",
                    &*format!("{name}.ext"),
                ]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
                args.extend(validity.iter().map(|arg| (*arg).to_owned()));
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                openssl(&dir, &refs);
            };
            leaf("server", "IP:127.0.0.1,DNS:localhost", &["-days", "2"]);
            leaf("ipalt", "IP:127.0.0.2,IP:::1", &["-days", "2"]);
            leaf("wronghost", "DNS:example.invalid", &["-days", "2"]);
            // A coordinator reached by a name of its own (the client's --server-name).
            leaf(
                "named",
                "IP:127.0.0.1,DNS:brain-coord.test",
                &["-days", "2"],
            );
            // Same SAN as the good server: only the validity window is wrong.
            leaf("expired", "IP:127.0.0.1,DNS:localhost", &["-days", "2"]);
            // The expired leaf is re-signed by the same CA through `openssl ca` with
            // fixed, ordered past dates: accepted by every OpenSSL 3.x, unlike
            // `x509 -not_before` (3.4+) or a negative `-days` (refused by 3.5).
            std::fs::create_dir_all(dir.join("ca-db")).expect("ca db");
            std::fs::write(dir.join("ca-db/index.txt"), b"").expect("ca index");
            std::fs::write(dir.join("ca-db/serial"), b"1000\n").expect("ca serial");
            std::fs::write(
                dir.join("ca-db/ca.cnf"),
                "[ca]\ndefault_ca = fixture\n[fixture]\ndatabase = ca-db/index.txt\n\
                 new_certs_dir = ca-db\nserial = ca-db/serial\ndefault_md = sha256\n\
                 policy = anything\nunique_subject = no\ncopy_extensions = none\n\
                 [anything]\ncommonName = optional\n",
            )
            .expect("ca config");
            openssl(
                &dir,
                &[
                    "ca",
                    "-batch",
                    "-notext",
                    "-config",
                    "ca-db/ca.cnf",
                    "-cert",
                    "ca.crt",
                    "-keyfile",
                    "ca.key",
                    "-in",
                    "expired.csr",
                    "-out",
                    "expired.crt",
                    "-extfile",
                    "expired.ext",
                    "-startdate",
                    "200101000000Z",
                    "-enddate",
                    "210101000000Z",
                ],
            );
            for key in ["ca", "other-ca", "server", "wronghost", "expired", "ipalt"] {
                std::fs::set_permissions(
                    dir.join(format!("{key}.key")),
                    std::fs::Permissions::from_mode(0o600),
                )
                .expect("key perm");
            }
            dir
        })
    }

    fn openssl(dir: &Path, args: &[&str]) {
        let output = Command::new("openssl")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("openssl binary is required");
        assert!(output.status.success(), "openssl {:?} failed", args.first());
    }

    fn file(name: &str) -> PathBuf {
        material().join(name)
    }

    fn temp_dir(_tag: &str) -> PathBuf {
        let dir = crate::test_support::temp_path("brain-tls");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("perm");
    }

    /// Test-only mkfifo via the local binary with a fixed argv (no unsafe).
    fn mkfifo(path: &Path) {
        let status = Command::new("mkfifo")
            .arg("-m")
            .arg("0600")
            .arg(path)
            .status()
            .expect("mkfifo binary");
        assert!(status.success(), "mkfifo failed");
    }

    fn start_with(tag: &str, leaf: &str, deadline: Duration) -> (TlsServerHandle, PathBuf) {
        let dir = temp_dir(tag);
        let mut config = TlsServerConfig::new(
            &dir,
            0,
            file(&format!("{leaf}.crt")),
            file(&format!("{leaf}.key")),
        );
        config.deadline = deadline;
        let handle = TlsCoordServer::start(&config)
            .expect("start tls server")
            .serve()
            .expect("serve");
        (handle, dir)
    }

    fn start(tag: &str, leaf: &str) -> (TlsServerHandle, PathBuf) {
        start_with(tag, leaf, MAX_DEADLINE)
    }

    fn issue(
        handle: &TlsServerHandle,
        machine: &str,
        agent: &str,
        role: &str,
        spaces: &[&str],
    ) -> String {
        let owned: Vec<String> = spaces.iter().map(|space| (*space).to_owned()).collect();
        handle
            .auth_admin(move |auth| {
                let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
                auth.upsert_principal(machine, agent, machine, role, &refs)?;
                auth.issue_credential(machine, agent, None)
                    .map(|token| token.as_str().to_owned())
            })
            .expect("issue")
    }

    fn client_for(
        handle: &TlsServerHandle,
        dir: &Path,
        name: &str,
        token: &str,
        ca: &str,
        host: &str,
    ) -> TlsCoordClient {
        let cred = dir.join(name);
        crate::network::client::write_credential_file(&cred, token).expect("write cred");
        TlsCoordClient::new(
            TlsClientConfig::new(file(ca), host, loopback(handle.port())),
            &cred,
        )
        .expect("client")
    }

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    }

    fn args(pairs: &[(&str, Value)]) -> serde_json::Map<String, Value> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    /// A syntactically valid but unknown credential: the TLS negatives
    /// must fail at the handshake, before the token is ever sent.
    fn probe(handle: &TlsServerHandle, ca: &str, host: &str) -> TlsCallResult {
        let dir = temp_dir(&format!("probe-{}-{ca}-{host}", handle.port()));
        let client = client_for(handle, &dir, "cred", "brc1.probe.deadbeef", ca, host);
        client.call("work_whoami", "", serde_json::Map::new(), None)
    }

    fn assert_handshake_refused(outcome: &TlsCallResult) {
        assert!(
            matches!(outcome, TlsCallResult::Tls(error) if error.code() == "tls_failure"),
            "expected a refused handshake, got {outcome:?}"
        );
    }

    fn config_error(config: &TlsServerConfig) -> TlsError {
        match server_config(config) {
            Ok(_) => panic!("expected a configuration error"),
            Err(error) => error,
        }
    }

    // ---- material and configuration negatives -------------------------

    #[test]
    fn missing_key_is_invalid_config() {
        let dir = temp_dir("nokey");
        let config = TlsServerConfig::new(&dir, 0, file("server.crt"), dir.join("absent.key"));
        assert_eq!(config_error(&config).code(), "invalid_config");
        assert!(
            TlsCoordServer::start(&config).is_err(),
            "no server without material"
        );
    }

    #[test]
    fn symlinked_key_is_refused() {
        let dir = temp_dir("symlinkkey");
        let link = dir.join("link.key");
        symlink(file("server.key"), &link).expect("symlink");
        let config = TlsServerConfig::new(&dir, 0, file("server.crt"), &link);
        assert_eq!(config_error(&config).code(), "invalid_config");
    }

    #[test]
    fn world_readable_key_is_refused() {
        let dir = temp_dir("worldkey");
        let key = dir.join("world.key");
        std::fs::copy(file("server.key"), &key).expect("copy");
        mode(&key, 0o644);
        let config = TlsServerConfig::new(&dir, 0, file("server.crt"), &key);
        assert_eq!(config_error(&config).code(), "invalid_config");
        mode(&key, 0o600);
        assert!(
            server_config(&TlsServerConfig::new(&dir, 0, file("server.crt"), &key)).is_ok(),
            "0600 copy loads"
        );
    }

    #[test]
    fn fifo_key_and_fifo_credential_are_refused_without_hanging() {
        let dir = temp_dir("fifo");
        let fifo = dir.join("fifo");
        mkfifo(&fifo);
        let started = Instant::now();
        let config = TlsServerConfig::new(&dir, 0, file("server.crt"), &fifo);
        assert_eq!(config_error(&config).code(), "invalid_config");
        assert!(
            crate::network::client::read_credential(&fifo).is_err(),
            "a FIFO credential must be refused"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "opening a FIFO must not block"
        );
    }

    #[test]
    fn malformed_key_and_ca_are_invalid_config() {
        let dir = temp_dir("badpem");
        let key = dir.join("bad.key");
        std::fs::write(&key, b"not a pem key at all").expect("write");
        mode(&key, 0o600);
        assert_eq!(
            config_error(&TlsServerConfig::new(&dir, 0, file("server.crt"), &key)).code(),
            "invalid_config"
        );
        let ca = dir.join("bad-ca.crt");
        std::fs::write(&ca, b"not a certificate").expect("write");
        let Err(error) = client_config(&TlsClientConfig::new(&ca, "localhost", loopback(1))) else {
            panic!("bad CA");
        };
        assert_eq!(error.code(), "invalid_config");
    }

    #[test]
    fn deadline_configuration_is_bounded() {
        let dir = temp_dir("deadline-config");
        for bad in [
            Duration::ZERO,
            Duration::from_millis(1),
            MAX_DEADLINE + Duration::from_millis(1),
            Duration::MAX,
        ] {
            let mut config = TlsServerConfig::new(&dir, 0, file("server.crt"), file("server.key"));
            config.deadline = bad;
            assert!(
                TlsCoordServer::start(&config).is_err(),
                "server deadline {bad:?} must be refused"
            );
            let mut client = TlsClientConfig::new(file("ca.crt"), "localhost", loopback(1));
            client.deadline = bad;
            let cred = dir.join("cred");
            let _ = crate::network::client::write_credential_file(&cred, "brc1.t1.abc");
            assert!(
                TlsCoordClient::new(client, &cred).is_err(),
                "client deadline {bad:?} must be refused"
            );
        }
    }

    #[test]
    fn debug_output_never_carries_the_token() {
        let (handle, dir) = start("debug", "server");
        let client = client_for(
            &handle,
            &dir,
            "cred",
            "brc1.secretid.secretvalue",
            "ca.crt",
            "localhost",
        );
        let shown = format!("{client:?}");
        assert!(
            !shown.contains("secretvalue") && shown.contains("<redacted>"),
            "{shown}"
        );
    }

    // ---- real handshake negatives -------------------------------------

    #[test]
    fn wrong_ca_is_refused_by_a_real_handshake() {
        let (handle, _dir) = start("wrongca", "server");
        assert_handshake_refused(&probe(&handle, "other-ca.crt", "localhost"));
    }

    #[test]
    fn wrong_hostname_is_refused_by_a_real_handshake() {
        let (handle, _dir) = start("wronghost", "wronghost");
        assert_handshake_refused(&probe(&handle, "ca.crt", "localhost"));
        assert_handshake_refused(&probe(&handle, "ca.crt", "127.0.0.1"));
    }

    #[test]
    fn expired_certificate_is_refused_by_a_real_handshake() {
        let (handle, _dir) = start("expired", "expired");
        assert_handshake_refused(&probe(&handle, "ca.crt", "localhost"));
    }

    #[test]
    fn plaintext_http_to_the_tls_port_is_never_served() {
        let (handle, _dir) = start("plaintext", "server");
        let mut stream = TcpStream::connect(("127.0.0.1", handle.port())).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let _ = stream
            .write_all(b"POST /v1/call HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}");
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        assert!(
            !String::from_utf8_lossy(&response).contains("HTTP/"),
            "plaintext must never be answered in the clear"
        );
    }

    #[test]
    fn a_tls_client_against_a_plaintext_server_fails_closed() {
        let dir = temp_dir("plainserver");
        let plain = crate::network::CoordServer::start(&crate::network::ServerConfig::new(&dir, 0))
            .expect("plain");
        let port = plain.port();
        let _plain = plain.serve();
        let cred = dir.join("cred");
        crate::network::client::write_credential_file(&cred, "brc1.t1.abc").expect("cred");
        let client = TlsCoordClient::new(
            TlsClientConfig::new(file("ca.crt"), "localhost", loopback(port)),
            &cred,
        )
        .expect("client");
        let outcome = client.call("work_whoami", "", serde_json::Map::new(), None);
        assert!(
            matches!(outcome, TlsCallResult::Tls(_)),
            "no plaintext fallback: {outcome:?}"
        );
    }

    // ---- happy path, identity and authorization over TLS ---------------

    #[test]
    fn whoami_over_tls_by_name_and_by_ip_san() {
        let (handle, dir) = start("whoami", "server");
        let token = issue(&handle, "m-tls", "tlsagent", "worker", &["s"]);
        for host in ["localhost", "127.0.0.1"] {
            let client = client_for(
                &handle,
                &dir,
                &format!("cred-{host}"),
                &token,
                "ca.crt",
                host,
            );
            match client.call("work_whoami", "", serde_json::Map::new(), None) {
                TlsCallResult::Ok(body) => {
                    assert_eq!(body["principal"]["agent"], "tlsagent");
                    assert_eq!(body["principal"]["machine"], "m-tls");
                }
                other => panic!("whoami over TLS via {host}: {other:?}"),
            }
        }
    }

    #[test]
    fn cgnat_dial_targets_need_their_own_opt_in() {
        assert!(parse_dial_target_with("100.64.0.42:7443", false, false).is_err());
        assert!(parse_dial_target_with("100.64.0.42:7443", true, false).is_err());
        assert!(parse_dial_target_with("100.64.0.42:7443", false, true).is_ok());
        assert!(parse_dial_target_with("10.0.0.5:7443", false, true).is_err());
        assert!(parse_dial_target_with("10.0.0.5:7443", true, false).is_ok());
        assert!(parse_dial_target_lan("100.64.0.42:7443", true).is_err());
    }

    #[test]
    fn a_server_name_from_the_certificate_is_accepted_as_host() {
        let (handle, dir) = start("named-host", "named");
        let token = issue(&handle, "m-named", "named-agent", "worker", &["s"]);
        let client = client_for(
            &handle,
            &dir,
            "cred-named",
            &token,
            "ca.crt",
            "brain-coord.test",
        );
        match client.call("work_whoami", "", serde_json::Map::new(), None) {
            TlsCallResult::Ok(body) => assert_eq!(body["principal"]["agent"], "named-agent"),
            other => panic!("whoami by the certificate's DNS name: {other:?}"),
        }
    }

    #[test]
    fn only_names_the_certificate_is_valid_for_pass_the_host_check() {
        let pem = std::fs::read(file("named.crt")).expect("named cert");
        let leaf = parse_certs(&pem).expect("parse").remove(0);
        let accepts = |host: &str| crate::network::server::host_is_certificate_name(host, &leaf);
        assert!(accepts("brain-coord.test"));
        assert!(accepts("brain-coord.test:7443"));
        for refused in [
            "evil.test",
            "brain-coord.test.evil",
            "brain-coord.test:",
            "brain-coord.test:abc",
            "127.0.0.1",
            "[::1]:7443",
            "",
        ] {
            assert!(
                !accepts(refused),
                "{refused:?} must not pass as a certificate name"
            );
        }
    }

    #[test]
    fn identity_space_and_role_are_enforced_over_tls() {
        let (handle, dir) = start("authz", "server");
        let worker = issue(&handle, "m-worker", "alice", "worker", &["s"]);
        let reader = issue(&handle, "m-reader", "bob", "reader", &["s"]);
        let alice = client_for(&handle, &dir, "alice", &worker, "ca.crt", "localhost");
        let bob = client_for(&handle, &dir, "bob", &reader, "ca.crt", "localhost");
        let add = || {
            args(&[
                ("space", "s".into()),
                ("project", "p".into()),
                ("title", "t".into()),
            ])
        };
        // Same credential, spoofed agent label: refused, whatever the tool.
        assert!(matches!(
            alice.call("work_add", "mallory", add(), None),
            TlsCallResult::Refused(NetError::Forbidden(_))
        ));
        // A machine in the body is refused; identity comes from the credential.
        let mut spoof = add();
        spoof.insert("machine".into(), "m-reader".into());
        assert!(matches!(
            alice.call("work_add", "alice", spoof, None),
            TlsCallResult::Refused(_)
        ));
        // Space allowlist.
        let other = args(&[("space", "other".into())]);
        assert!(matches!(
            alice.call("work_board", "alice", other, None),
            TlsCallResult::Refused(NetError::Forbidden(_))
        ));
        // Reader reads but may not write.
        assert!(matches!(
            bob.call("work_board", "bob", args(&[("space", "s".into())]), None),
            TlsCallResult::Ok(_)
        ));
        assert!(matches!(
            bob.call("work_add", "bob", add(), None),
            TlsCallResult::Refused(NetError::Forbidden(_))
        ));
        // The worker's own write goes through.
        assert!(matches!(
            alice.call("work_add", "alice", add(), None),
            TlsCallResult::Ok(_)
        ));
    }

    #[test]
    fn revoked_credential_is_refused_before_the_cached_replay() {
        let (handle, dir) = start("revoke", "server");
        let token = issue(&handle, "m-tls", "tlsagent", "worker", &["s"]);
        let client = client_for(&handle, &dir, "cred", &token, "ca.crt", "localhost");
        let added = match client.call(
            "work_add",
            "tlsagent",
            args(&[
                ("space", "s".into()),
                ("project", "p".into()),
                ("title", "tls".into()),
            ]),
            None,
        ) {
            TlsCallResult::Ok(body) => body,
            other => panic!("work_add over TLS: {other:?}"),
        };
        let task_id = added["result"]["task"]["id"].as_i64().expect("task id");
        let claim = || {
            args(&[
                ("space", "s".into()),
                ("task_id", task_id.into()),
                ("ttl_secs", 600.into()),
            ])
        };
        let first = match client.call("work_claim", "tlsagent", claim(), Some("tls-1".into())) {
            TlsCallResult::Ok(body) => body,
            other => panic!("work_claim over TLS: {other:?}"),
        };
        assert_eq!(first["result"]["status"], "ok", "claim: {first}");
        // The replay is idempotent while the credential is live...
        match client.call("work_claim", "tlsagent", claim(), Some("tls-1".into())) {
            TlsCallResult::Ok(body) => assert_eq!(
                body["result"]["claim"]["claim_id"],
                first["result"]["claim"]["claim_id"]
            ),
            other => panic!("replay: {other:?}"),
        }
        handle
            .auth_admin(|auth| auth.disable_principal("m-tls"))
            .expect("revoke");
        // ...and refused by authentication, never answered from the cache.
        let replay = client.call("work_claim", "tlsagent", claim(), Some("tls-1".into()));
        assert!(
            matches!(replay, TlsCallResult::Refused(NetError::Unauthorized)),
            "{replay:?}"
        );
    }

    #[test]
    fn concurrent_tls_claims_have_exactly_one_winner() {
        let (handle, dir) = start("race", "server");
        let seed = issue(&handle, "m-seed", "seed", "worker", &["s"]);
        let seeder = client_for(&handle, &dir, "seed", &seed, "ca.crt", "localhost");
        let TlsCallResult::Ok(added) = seeder.call(
            "work_add",
            "seed",
            args(&[
                ("space", "s".into()),
                ("project", "p".into()),
                ("title", "race".into()),
            ]),
            None,
        ) else {
            panic!("seed task");
        };
        let task_id = added["result"]["task"]["id"].as_i64().expect("task id");
        let clients: Vec<TlsCoordClient> = (0..8)
            .map(|index| {
                let agent = format!("racer{index}");
                let token = issue(
                    &handle,
                    &format!("m-race-{index}"),
                    &agent,
                    "worker",
                    &["s"],
                );
                client_for(&handle, &dir, &agent, &token, "ca.crt", "localhost")
            })
            .collect();
        let wins = thread::scope(|scope| {
            let runs: Vec<_> = clients
                .iter()
                .enumerate()
                .map(|(index, client)| {
                    scope.spawn(move || {
                        let claim = args(&[
                            ("space", "s".into()),
                            ("task_id", task_id.into()),
                            ("ttl_secs", 600.into()),
                        ]);
                        match client.call("work_claim", &format!("racer{index}"), claim, None) {
                            TlsCallResult::Ok(body) => body["result"]["status"] == "ok",
                            other => panic!("racer {index}: {other:?}"),
                        }
                    })
                })
                .collect();
            runs.into_iter()
                .map(|run| run.join().expect("racer thread"))
                .filter(|won| *won)
                .count()
        });
        assert_eq!(wins, 1, "exactly one TLS client may win the claim");
    }

    // ---- absolute deadline and owned stop ------------------------------

    /// Opens a TLS record that declares 16 KiB and then trickles one byte
    /// every 50 ms: each read succeeds well inside any per-read timeout, so
    /// only an ABSOLUTE deadline can end it. Returns when the server hangs up.
    fn trickle_until_closed(port: u16, give_up: Duration) -> Duration {
        let started = Instant::now();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let _ = stream.write_all(&[0x16, 0x03, 0x01, 0x40, 0x00]);
        while started.elapsed() < give_up {
            thread::sleep(Duration::from_millis(50));
            if stream.write_all(&[0]).is_err() {
                break;
            }
        }
        started.elapsed()
    }

    #[test]
    fn slow_trickle_cannot_outlive_the_server_deadline() {
        let (handle, _dir) = start_with("trickle", "server", Duration::from_millis(400));
        let elapsed = trickle_until_closed(handle.port(), Duration::from_secs(10));
        assert!(
            elapsed < Duration::from_secs(3),
            "the trickling peer must be cut at the deadline, took {elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(350),
            "closed before the deadline: {elapsed:?}"
        );
        let settle = Instant::now();
        while handle.live_connections() > 0 && settle.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            handle.live_connections(),
            0,
            "the connection thread must end"
        );
    }

    #[test]
    fn slow_trickling_server_cannot_outlive_the_client_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let started = Instant::now();
            let _ = stream.write_all(&[0x16, 0x03, 0x03, 0x40, 0x00]);
            while started.elapsed() < Duration::from_secs(10) {
                thread::sleep(Duration::from_millis(50));
                if stream.write_all(&[0]).is_err() {
                    break;
                }
            }
        });
        let dir = temp_dir("client-trickle");
        let cred = dir.join("cred");
        crate::network::client::write_credential_file(&cred, "brc1.t1.abc").expect("cred");
        let mut config = TlsClientConfig::new(file("ca.crt"), "localhost", loopback(port));
        config.deadline = Duration::from_millis(400);
        let client = TlsCoordClient::new(config, &cred).expect("client");
        let started = Instant::now();
        let outcome = client.call("work_whoami", "", serde_json::Map::new(), None);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "client must give up at its deadline, took {elapsed:?}"
        );
        assert!(
            matches!(outcome, TlsCallResult::Tls(TlsError::Unreachable(_))),
            "{outcome:?}"
        );
        drop(client);
        peer.join().expect("peer");
    }

    #[test]
    fn stop_cancels_stalled_connections_and_joins_them_quickly() {
        let (handle, _dir) = start("stop", "server");
        let port = handle.port();
        // Three peers that connect and then say nothing: with the default
        // 5 s deadline they would hold their threads for 5 s.
        let stalled: Vec<TcpStream> = (0..3)
            .map(|_| TcpStream::connect(("127.0.0.1", port)).expect("connect"))
            .collect();
        let settle = Instant::now();
        while handle.live_connections() < 3 && settle.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(handle.live_connections(), 3);
        let started = Instant::now();
        let joined = handle.stop();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "stop must cancel, not wait out the deadline: {:?}",
            started.elapsed()
        );
        assert_eq!(joined, 3, "every owned connection thread is joined");
        drop(stalled);
        // Nothing listens any more: the port no longer completes a TLS call.
        assert!(
            TcpStream::connect(("127.0.0.1", port)).is_err(),
            "listener closed after stop"
        );
    }

    // ---- no name resolution on the bounded path -----------------------

    /// Records every dial and delegates to the real socket after `delay`.
    struct RecordingDialer {
        delay: Duration,
        dials: Mutex<Vec<(SocketAddr, Duration)>>,
    }

    impl Dialer for RecordingDialer {
        fn dial(&self, address: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
            self.dials.lock().expect("dials").push((address, timeout));
            if !self.delay.is_zero() {
                // A well-behaved slow dialer: it never waits past what it was given.
                thread::sleep(self.delay.min(timeout));
                return Err(io::Error::new(io::ErrorKind::TimedOut, "slow dial"));
            }
            TcpStream::connect_timeout(&address, timeout)
        }
    }

    #[test]
    fn dial_targets_are_numeric_loopback_and_never_resolved() {
        for refused in [
            "localhost:7000",
            "example.invalid:443",
            "brain.local:1",
            "127.0.0.1",
            "127.0.0.1:0",
            "10.0.0.1:7000",
            "192.168.1.5:7000",
            "0.0.0.0:7000",
            "[::]:7000",
            "",
        ] {
            let error = parse_dial_target(refused)
                .err()
                .unwrap_or_else(|| panic!("{refused:?} must be refused"));
            assert_eq!(error.code(), "invalid_config", "{refused:?}");
        }
        assert_eq!(
            parse_dial_target("127.0.0.1:7000").expect("v4"),
            loopback(7000)
        );
        assert!(
            parse_dial_target("[::1]:7000")
                .expect("v6")
                .ip()
                .is_loopback()
        );
        assert!(
            parse_dial_target("127.8.9.10:7000").is_ok(),
            "all of 127/8 is loopback"
        );
        // A non-loopback SocketAddr cannot sneak in through the config either.
        let dir = temp_dir("dial-lan");
        let cred = dir.join("cred");
        crate::network::client::write_credential_file(&cred, "brc1.t1.abc").expect("cred");
        let lan = TlsClientConfig::new(
            file("ca.crt"),
            "localhost",
            SocketAddr::from(([10, 0, 0, 1], 7000)),
        );
        assert!(
            TlsCoordClient::new(lan, &cred).is_err(),
            "LAN dial is a separate gate"
        );
    }

    #[test]
    fn the_bounded_path_dials_the_numeric_target_with_only_the_time_left() {
        let (handle, dir) = start("dialer", "server");
        let token = issue(&handle, "m-dial", "dialer", "worker", &["s"]);
        let cred = dir.join("cred");
        crate::network::client::write_credential_file(&cred, &token).expect("cred");
        let recorder = Arc::new(RecordingDialer {
            delay: Duration::ZERO,
            dials: Mutex::new(Vec::new()),
        });
        // The certificate name and the dial target are independent.
        let config = TlsClientConfig::new(file("ca.crt"), "localhost", loopback(handle.port()));
        let client =
            TlsCoordClient::with_dialer(config, &KeySource::File(cred.clone()), recorder.clone())
                .expect("client");
        assert!(matches!(
            client.call("work_whoami", "", serde_json::Map::new(), None),
            TlsCallResult::Ok(_)
        ));
        let dials = recorder.dials.lock().expect("dials").clone();
        assert_eq!(dials.len(), 1, "exactly one dial, no address list");
        assert_eq!(
            dials[0].0,
            loopback(handle.port()),
            "the numeric target, unresolved"
        );
        assert!(
            dials[0].1 <= MAX_DEADLINE && !dials[0].1.is_zero(),
            "dial gets the time left: {:?}",
            dials[0].1
        );
    }

    #[test]
    fn a_slow_dial_cannot_outlive_the_client_deadline() {
        let dir = temp_dir("slowdial");
        let cred = dir.join("cred");
        crate::network::client::write_credential_file(&cred, "brc1.t1.abc").expect("cred");
        let slow = Arc::new(RecordingDialer {
            delay: Duration::from_secs(30),
            dials: Mutex::new(Vec::new()),
        });
        let mut config = TlsClientConfig::new(file("ca.crt"), "localhost", loopback(9));
        config.deadline = Duration::from_millis(300);
        let client =
            TlsCoordClient::with_dialer(config, &KeySource::File(cred.clone()), slow.clone())
                .expect("client");
        let started = Instant::now();
        let outcome = client.call("work_whoami", "", serde_json::Map::new(), None);
        let elapsed = started.elapsed();
        assert!(
            matches!(outcome, TlsCallResult::Tls(TlsError::Unreachable(_))),
            "nothing sent: {outcome:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "bounded by the 300 ms deadline, took {elapsed:?}"
        );
        let given = slow.dials.lock().expect("dials")[0].1;
        assert!(
            given <= Duration::from_millis(300),
            "dialer got more than the deadline: {given:?}"
        );
    }

    /// In-memory duplex stream for driving the core router directly.
    struct Duplex {
        input: io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn no_dispatch_after_stop() {
        let dir = temp_dir("nodispatch");
        let core = Inner::open(&dir).expect("core");
        let token = core
            .auth_admin(|auth| {
                auth.upsert_principal("m-stop", "a", "m-stop", "worker", &["s"])?;
                auth.issue_credential("m-stop", "a", None)
                    .map(|token| token.as_str().to_owned())
            })
            .expect("issue");
        let body = r#"{"tool":"work_add","agent":"a","arguments":{"space":"s","project":"p","title":"late"}}"#;
        let request = format!(
            "POST /v1/call HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let run = |core: &Inner| {
            let mut duplex = Duplex {
                input: io::Cursor::new(request.clone().into_bytes()),
                output: Vec::new(),
            };
            core.serve_stream(&mut duplex);
            String::from_utf8_lossy(&duplex.output)
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned()
        };
        assert_eq!(run(&core), "200", "served while running");
        core.stop();
        assert_eq!(run(&core), "503", "refused before dispatch after stop");
    }

    #[test]
    fn lan_listen_and_dial_need_the_opt_in_and_refuse_wide_addresses() {
        let dir = crate::test_support::temp_path("brain-lan-start");
        let refused = |listen: &str, allow_lan: bool| {
            let mut config =
                TlsServerConfig::new(&dir, 0, dir.join("absent.crt"), dir.join("absent.key"));
            config.listen = listen.parse().expect("ip");
            config.allow_lan = allow_lan;
            let error = TlsCoordServer::start(&config).err().expect("refused");
            assert_eq!(error.status(), 400, "{listen}");
            error.body().to_string()
        };
        // Checked before any material or store: the error names the policy.
        assert!(refused("192.168.77.10", false).contains("opt-in"));
        for wide in ["0.0.0.0", "::", "224.0.0.1", "255.255.255.255", "8.8.8.8"] {
            assert!(refused(wide, true).contains("private unicast"), "{wide}");
        }
        assert!(!dir.exists(), "no state was created for a refused bind");

        assert!(parse_dial_target("192.168.77.10:7443").is_err());
        assert!(parse_dial_target_lan("192.168.77.10:7443", false).is_err());
        assert_eq!(
            parse_dial_target_lan("192.168.77.10:7443", true).expect("lan"),
            "192.168.77.10:7443".parse::<SocketAddr>().expect("addr")
        );
        assert!(parse_dial_target_lan("[fd00::17]:7443", true).is_ok());
        for refused in [
            "0.0.0.0:7443",
            "[::]:7443",
            "224.0.0.1:7443",
            "8.8.8.8:7443",
            "192.168.77.10:0",
            "brain.lan:7443",
        ] {
            assert!(parse_dial_target_lan(refused, true).is_err(), "{refused}");
        }
        assert!(parse_dial_target_lan("127.0.0.1:7443", false).is_ok());
    }

    /// An explicit non-default loopback bind, an IP-only certificate and a
    /// `--server-name` equal to the listen IP work end to end over real TLS;
    /// credential, role and space rights are unchanged.
    #[test]
    fn explicit_bound_ip_works_with_matching_ip_san_and_server_name() {
        for listen in ["127.0.0.2", "::1"] {
            let ip: IpAddr = listen.parse().expect("ip");
            let dir = temp_dir(&format!("bound-{}", listen.replace(':', "_")));
            let mut config = TlsServerConfig::new(&dir, 0, file("ipalt.crt"), file("ipalt.key"));
            config.listen = ip;
            let server = match TlsCoordServer::start(&config) {
                Ok(server) => server,
                Err(error) if ip.is_ipv6() && error.status() != 400 => {
                    eprintln!("skipping ::1: IPv6 loopback unavailable here ({error:?})");
                    continue;
                }
                Err(error) => panic!("start on {listen}: {error:?}"),
            };
            let handle = server.serve().expect("serve");
            let token = issue(&handle, "m-a", "alice", "reader", &["s"]);
            let cred = dir.join("alice");
            crate::network::client::write_credential_file(&cred, &token).expect("cred");
            let dial = SocketAddr::new(ip, handle.port());
            let client =
                TlsCoordClient::new(TlsClientConfig::new(file("ca.crt"), listen, dial), &cred)
                    .expect("client");
            match client.call("work_whoami", "", serde_json::Map::new(), None) {
                TlsCallResult::Ok(envelope) => {
                    assert!(envelope.to_string().contains("alice"), "{envelope}");
                }
                other => panic!("{listen}: whoami failed: {other:?}"),
            }
            // Rights are unchanged: a reader cannot write, nor reach another space.
            let add = args(&[
                ("space", "other".into()),
                ("project", "p".into()),
                ("title", "t".into()),
            ]);
            assert!(
                !matches!(client.call("work_add", "", add, None), TlsCallResult::Ok(_)),
                "{listen}: reader write to an ungranted space must be refused"
            );
            // A name the certificate does not carry still fails the handshake.
            let wrong = TlsCoordClient::new(
                TlsClientConfig::new(file("ca.crt"), "localhost", dial),
                &cred,
            )
            .expect("client");
            assert!(matches!(
                wrong.call("work_whoami", "", serde_json::Map::new(), None),
                TlsCallResult::Tls(_)
            ));
            drop(handle);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
