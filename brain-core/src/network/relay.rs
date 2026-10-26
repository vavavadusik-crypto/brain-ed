//! Opt-in company-hosted relay. Outer pinned TLS protects relay credentials;
//! the forwarded payload remains coordinator TLS. No payload is persisted.
use super::auth::{AuthStore, BearerToken};
use super::tls::{self, Dialer, KeySource, TlsClientConfig, TlsServerConfig, Transport};
use rustls::{ClientConnection, ServerConnection, StreamOwned};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const POLL: Duration = Duration::from_millis(10);
const AUTH_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_HEADER: usize = 2048;
const PER_NAME: usize = 4;
// Enforce one absolute deadline even if rustls performs several socket
// reads internally. An outer TLS slow drip cannot extend the inner deadline.
struct BoundedSocket {
    tcp: TcpStream,
    deadline: Cell<Option<Instant>>,
}
impl BoundedSocket {
    fn new(tcp: TcpStream, end: Instant) -> Self {
        Self {
            tcp,
            deadline: Cell::new(Some(end)),
        }
    }
    fn before_io(&self) -> io::Result<()> {
        if let Some(end) = self.deadline.get() {
            let left = end
                .checked_duration_since(Instant::now())
                .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
            self.tcp.set_timeout(left)?;
        }
        Ok(())
    }
    fn set_nonblocking(&self, enabled: bool) -> io::Result<()> {
        self.tcp.set_nonblocking(enabled)?;
        if enabled {
            self.deadline.set(None);
        }
        Ok(())
    }
}
impl std::ops::Deref for BoundedSocket {
    type Target = TcpStream;
    fn deref(&self) -> &TcpStream {
        &self.tcp
    }
}
impl Read for BoundedSocket {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.before_io()?;
        self.tcp.read(b)
    }
}
impl Write for BoundedSocket {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.before_io()?;
        self.tcp.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.before_io()?;
        self.tcp.flush()
    }
}
impl Transport for BoundedSocket {
    fn set_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.deadline.set(Some(Instant::now() + timeout));
        self.tcp.set_timeout(timeout)
    }
}
type ServerTls = StreamOwned<ServerConnection, BoundedSocket>;
type ClientTls = StreamOwned<ClientConnection, BoundedSocket>;
fn bad() -> io::Error {
    io::Error::other("relay refused or unavailable")
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn valid_name(name: &str) -> bool {
    (3..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Relay-only store. It never opens the coordinator auth database.
pub fn auth_store(state: &Path) -> io::Result<AuthStore> {
    let dir = state.join("relay-auth");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    for path in [&dir, &dir.join("auth.sqlite")] {
        match std::fs::symlink_metadata(path) {
            Ok(m)
                if !m.file_type().is_symlink()
                    && (m.is_dir() || m.is_file())
                    && Some(m.uid()) == tls::current_uid()
                    && m.mode() & 0o077 == 0 => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            _ => return Err(bad()),
        }
    }
    AuthStore::open(&dir).map_err(|_| bad())
}

/// Explicit relay host (DNS allowed here only). Resolution happens once at
/// configuration, never for the coordinator SNI. DNS results are bounded.
pub fn relay_host(target: &str) -> io::Result<String> {
    if let Ok(address) = target.parse::<SocketAddr>() {
        if address.port() == 0 || address.ip().is_unspecified() || address.ip().is_multicast() {
            return Err(bad());
        }
        return Ok(address.ip().to_string());
    }
    let (host, port) = target.rsplit_once(':').ok_or_else(bad)?;
    if port.parse::<u16>().ok().is_none_or(|p| p == 0)
        || host.len() > 253
        || host.is_empty()
        || !host.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
        })
    {
        return Err(bad());
    }
    Ok(host.into())
}
pub fn relay_address(host: &str) -> io::Result<SocketAddr> {
    relay_host(host)?;
    let address = host
        .to_socket_addrs()?
        .take(8)
        .find(|a| a.port() != 0 && !a.ip().is_unspecified() && !a.ip().is_multicast())
        .ok_or_else(bad)?;
    Ok(address)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub address: String,
    pub name: String,
    pub server_name: String,
    pub ca: PathBuf,
    pub token_file: PathBuf,
}
impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayRoute")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}
#[derive(Clone)]
pub struct Connector {
    address: SocketAddr,
    name: String,
    server_name: rustls::pki_types::ServerName<'static>,
    tls: Arc<rustls::ClientConfig>,
    token: String,
}
impl Connector {
    pub fn new(route: &Route) -> io::Result<Self> {
        if !valid_name(&route.name) {
            return Err(bad());
        }
        let address = relay_address(&route.address)?;
        let config = TlsClientConfig::new(&route.ca, &route.server_name, address);
        Ok(Self {
            address,
            name: route.name.clone(),
            server_name: route.server_name.clone().try_into().map_err(|_| bad())?,
            tls: Arc::new(tls::client_config(&config).map_err(|_| bad())?),
            token: super::client::read_credential_from(&KeySource::File(route.token_file.clone()))
                .map_err(|_| bad())?,
        })
    }
    fn open(&self, role: &str, timeout: Duration) -> io::Result<ClientTls> {
        let end = Instant::now() + timeout;
        let socket = TcpStream::connect_timeout(&self.address, timeout)?;
        socket.set_nodelay(true)?;
        let conn =
            ClientConnection::new(self.tls.clone(), self.server_name.clone()).map_err(|_| bad())?;
        let mut stream = StreamOwned::new(conn, BoundedSocket::new(socket, end));
        while stream.conn.is_handshaking() {
            set_remaining(&stream.sock, end)?;
            stream.conn.complete_io(&mut stream.sock)?;
        }
        set_remaining(&stream.sock, end)?;
        let header = serde_json::json!({"protocol":"brain-relay/1","role":role,"name":self.name,"token":self.token});
        let bytes = serde_json::to_vec(&header).map_err(|_| bad())?;
        stream.write_all(&(bytes.len() as u16).to_be_bytes())?;
        stream.write_all(&bytes)?;
        stream.flush()?;
        read_signal(&mut stream, end, b'R')?;
        if role == "connect" {
            read_signal(&mut stream, end, b'G')?;
        }
        Ok(stream)
    }
}
fn set_remaining(sock: &BoundedSocket, end: Instant) -> io::Result<()> {
    let remaining = end.checked_duration_since(Instant::now()).ok_or_else(bad)?;
    sock.set_timeout(remaining)
}
fn read_signal(stream: &mut ClientTls, end: Instant, expected: u8) -> io::Result<()> {
    set_remaining(&stream.sock, end)?;
    let mut byte = [0];
    stream.read_exact(&mut byte)?;
    if byte[0] != expected {
        return Err(bad());
    }
    Ok(())
}
struct Tunnel(ClientTls);
impl Read for Tunnel {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.read(b)
    }
}
impl Write for Tunnel {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
impl Transport for Tunnel {
    fn set_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.0.sock.set_timeout(timeout)
    }
}
/// Direct connect failure alone enables relay fallback. A TLS refusal or an
/// ambiguous write never causes replay through a second route.
pub struct RelayDialer {
    pub direct: bool,
    pub connector: Connector,
}
impl Dialer for RelayDialer {
    fn dial(&self, _: SocketAddr, _: Duration) -> io::Result<TcpStream> {
        Err(bad())
    }
    fn open(&self, address: SocketAddr, timeout: Duration) -> io::Result<Box<dyn Transport>> {
        let end = Instant::now() + timeout;
        if self.direct
            && let Ok(socket) = TcpStream::connect_timeout(&address, timeout / 3)
        {
            return Ok(Box::new(socket));
        }
        let left = end.checked_duration_since(Instant::now()).ok_or_else(bad)?;
        Ok(Box::new(Tunnel(self.connector.open("connect", left)?)))
    }
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub connections: usize,
    /// Per tunnel, combined directions; bounded buffers add at most 32 KiB burst.
    pub bytes_per_second: u64,
    pub idle: Duration,
    pub lifetime: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            connections: 128,
            bytes_per_second: 4 * 1024 * 1024,
            idle: Duration::from_secs(30),
            lifetime: Duration::from_secs(300),
        }
    }
}
impl Limits {
    fn validate(&self) -> io::Result<()> {
        if !(2..=1024).contains(&self.connections)
            || !(1024..=1024 * 1024 * 1024).contains(&self.bytes_per_second)
            || self.idle < Duration::from_secs(1)
            || self.idle > Duration::from_secs(300)
            || self.lifetime < self.idle
            || self.lifetime > Duration::from_secs(3600)
        {
            return Err(bad());
        }
        Ok(())
    }
}
struct Shared {
    stop: AtomicBool,
    sockets: Mutex<BTreeMap<u64, TcpStream>>,
    next: AtomicU64,
}
impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            sockets: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(1),
        })
    }
    fn track(self: &Arc<Self>, sock: &TcpStream, cap: usize) -> io::Result<Lease> {
        let mut sockets = self.sockets.lock().map_err(|_| bad())?;
        if sockets.len() >= cap || self.stop.load(Ordering::SeqCst) {
            return Err(bad());
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        sockets.insert(id, sock.try_clone()?);
        Ok(Lease {
            id,
            shared: self.clone(),
        })
    }
    fn cancel(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(sockets) = self.sockets.lock() {
            for sock in sockets.values() {
                let _ = sock.shutdown(Shutdown::Both);
            }
        }
    }
}
struct Lease {
    id: u64,
    shared: Arc<Shared>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut sockets) = self.shared.sockets.lock() {
            sockets.remove(&self.id);
        }
    }
}
struct Incoming {
    stream: ServerTls,
    lease: Lease,
    token: BearerToken,
}
struct Waiting {
    id: u64,
    sender: mpsc::SyncSender<Incoming>,
}
type Registry = Arc<Mutex<BTreeMap<String, Vec<Waiting>>>>;
struct PendingRegistration {
    registry: Registry,
    name: String,
    id: u64,
}
impl Drop for PendingRegistration {
    fn drop(&mut self) {
        if let Ok(mut all) = self.registry.lock()
            && let Some(queue) = all.get_mut(&self.name)
        {
            queue.retain(|w| w.id != self.id);
            if queue.is_empty() {
                all.remove(&self.name);
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    protocol: String,
    role: String,
    name: String,
    token: String,
}

pub struct RelayServer {
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    state: PathBuf,
    limits: Limits,
}
impl RelayServer {
    pub fn start(state: &Path, listen: IpAddr, port: u16, limits: Limits) -> io::Result<Self> {
        limits.validate()?;
        // Public/wildcard listening is explicit only on this relay command.
        if listen.is_multicast() {
            return Err(bad());
        }
        auth_store(state)?;
        let config = TlsServerConfig::new(
            state,
            port,
            state.join("tls/server.pem"),
            state.join("tls/server.key"),
        );
        let tls = Arc::new(tls::server_config(&config).map_err(|_| bad())?);
        let listener = TcpListener::bind(SocketAddr::new(listen, port))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            tls,
            state: state.to_owned(),
            limits,
        })
    }
    pub fn address(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
    pub fn serve(self) -> io::Result<RelayHandle> {
        let shared = Shared::new();
        let flag = shared.clone();
        let join = thread::Builder::new()
            .name("brain-relay".into())
            .spawn(move || {
                let registry: Registry = Arc::default();
                let mut workers = Vec::new();
                while !flag.stop.load(Ordering::SeqCst) {
                    workers.retain(|w: &JoinHandle<()>| !w.is_finished());
                    let Ok((socket, _)) = self.listener.accept() else {
                        thread::sleep(POLL);
                        continue;
                    };
                    let Ok(lease) = flag.track(&socket, self.limits.connections) else {
                        continue;
                    };
                    let (tls, state, limits, registry) = (
                        self.tls.clone(),
                        self.state.clone(),
                        self.limits.clone(),
                        registry.clone(),
                    );
                    let flag = flag.clone();
                    if let Ok(worker) = thread::Builder::new()
                        .name("brain-relay-conn".into())
                        .spawn(move || {
                            let result = relay_connection(
                                socket, lease, tls, &state, &limits, &registry, &flag,
                            );
                            // Fixed categories only: no headers, tokens, names or payload.
                            eprintln!(
                                "{{\"event\":\"relay_connection\",\"outcome\":\"{}\"}}",
                                if result.is_ok() {
                                    "closed"
                                } else {
                                    "refused_or_closed"
                                }
                            );
                        })
                    {
                        workers.push(worker);
                    }
                }
                flag.cancel();
                for worker in workers {
                    let _ = worker.join();
                }
            })?;
        Ok(RelayHandle {
            shared,
            join: Some(join),
        })
    }
}
pub struct RelayHandle {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}
impl Drop for RelayHandle {
    fn drop(&mut self) {
        self.shared.cancel();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
fn authorized(store: &AuthStore, token: &BearerToken, name: &str, role: &str) -> bool {
    store
        .authenticate(token, now(), "relay")
        .is_ok_and(|a| a.principal.machine_id == name && a.agent_id() == role)
}
fn relay_connection(
    socket: TcpStream,
    lease: Lease,
    config: Arc<rustls::ServerConfig>,
    state: &Path,
    limits: &Limits,
    registry: &Registry,
    flag: &Shared,
) -> io::Result<()> {
    let end = Instant::now() + AUTH_TIMEOUT;
    socket.set_nodelay(true)?;
    let mut stream = StreamOwned::new(
        ServerConnection::new(config).map_err(|_| bad())?,
        BoundedSocket::new(socket, end),
    );
    while stream.conn.is_handshaking() {
        set_remaining(&stream.sock, end)?;
        stream.conn.complete_io(&mut stream.sock)?;
    }
    // Bytewise bounded header prevents slow drip from extending auth deadline.
    let mut len = [0; 2];
    for byte in &mut len {
        set_remaining(&stream.sock, end)?;
        stream.read_exact(std::slice::from_mut(byte))?;
    }
    let len = u16::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_HEADER {
        return Err(bad());
    }
    let mut bytes = vec![0; len];
    for byte in &mut bytes {
        set_remaining(&stream.sock, end)?;
        stream.read_exact(std::slice::from_mut(byte))?;
    }
    let header: Header = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    if header.protocol != "brain-relay/1"
        || !valid_name(&header.name)
        || !["connect", "register"].contains(&header.role.as_str())
    {
        return Err(bad());
    }
    let token = BearerToken::parse(&header.token).map_err(|_| bad())?;
    let store = auth_store(state)?;
    if !authorized(&store, &token, &header.name, &header.role) {
        return Err(bad());
    }
    if header.role == "connect" {
        stream.write_all(b"R")?;
        stream.flush()?;
        let pending = registry
            .lock()
            .map_err(|_| bad())?
            .get_mut(&header.name)
            .and_then(|q| q.pop())
            .ok_or_else(bad)?;
        pending
            .sender
            .send(Incoming {
                stream,
                lease,
                token,
            })
            .map_err(|_| bad())?;
        return Ok(());
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let id = lease.id;
    {
        let mut all = registry.lock().map_err(|_| bad())?;
        let queue = all.entry(header.name.clone()).or_default();
        if queue.len() >= PER_NAME {
            return Err(bad());
        }
        queue.push(Waiting { id, sender: tx });
    }
    let pending = PendingRegistration {
        registry: registry.clone(),
        name: header.name.clone(),
        id,
    };
    stream.write_all(b"R")?;
    stream.flush()?;
    stream.sock.set_nonblocking(true)?;
    let waiting_since = Instant::now();
    let incoming = loop {
        if flag.stop.load(Ordering::SeqCst)
            || waiting_since.elapsed() >= limits.idle
            || !authorized(&store, &token, &header.name, "register")
        {
            break Err(bad());
        }
        let mut peek = [0];
        match stream.sock.peek(&mut peek) {
            Ok(_) => break Err(bad()), // EOF or unsolicited bytes before pairing
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => break Err(bad()),
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(incoming) => break Ok(incoming),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break Err(bad()),
        }
    };
    drop(pending);
    let mut incoming = incoming?;
    // Recheck both credentials after queueing and throughout the tunnel.
    let check = || {
        !flag.stop.load(Ordering::SeqCst)
            && authorized(&store, &token, &header.name, "register")
            && authorized(&store, &incoming.token, &header.name, "connect")
    };
    if !check() {
        return Err(bad());
    }
    stream.sock.set_nonblocking(false)?;
    stream.sock.set_timeout(AUTH_TIMEOUT)?;
    incoming.stream.sock.set_timeout(AUTH_TIMEOUT)?;
    stream.write_all(b"G")?;
    stream.flush()?;
    incoming.stream.write_all(b"G")?;
    incoming.stream.flush()?;
    stream.sock.set_nonblocking(true)?;
    incoming.stream.sock.set_nonblocking(true)?;
    let result = bridge(&mut stream, &mut incoming.stream, limits, check);
    finish_server(&mut stream);
    finish_server(&mut incoming.stream);
    drop(incoming.lease);
    result
}
fn finish_server(stream: &mut ServerTls) {
    stream.conn.send_close_notify();
    let _ = stream.flush();
    let _ = stream.sock.shutdown(Shutdown::Both);
}
fn transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}
struct Buffer {
    bytes: [u8; 16384],
    start: usize,
    end: usize,
    eof: bool,
}
impl Buffer {
    fn new() -> Self {
        Self {
            bytes: [0; 16384],
            start: 0,
            end: 0,
            eof: false,
        }
    }
    fn transfer(
        &mut self,
        from: &mut impl Read,
        to: &mut impl Write,
        allowance: usize,
    ) -> io::Result<usize> {
        if self.start == self.end && !self.eof && allowance > 0 {
            match from.read(&mut self.bytes[..allowance.min(16384)]) {
                Ok(0) => self.eof = true,
                Ok(n) => {
                    self.start = 0;
                    self.end = n;
                }
                Err(e) if transient(&e) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => self.eof = true,
                Err(e) => return Err(e),
            }
        }
        if self.start < self.end {
            match to.write(&self.bytes[self.start..self.end]) {
                Ok(0) => return Err(bad()),
                Ok(n) => {
                    self.start += n;
                    return Ok(n);
                }
                Err(e) if transient(&e) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(0)
    }
}
trait BridgeStream: Read + Write {
    fn close_write(&mut self) -> io::Result<()>;
}
impl BridgeStream for TcpStream {
    fn close_write(&mut self) -> io::Result<()> {
        self.shutdown(Shutdown::Write)
    }
}
impl BridgeStream for ClientTls {
    fn close_write(&mut self) -> io::Result<()> {
        self.conn.send_close_notify();
        self.flush()
    }
}
impl BridgeStream for ServerTls {
    fn close_write(&mut self) -> io::Result<()> {
        self.conn.send_close_notify();
        self.flush()
    }
}
/// Bounded duplex copying with backpressure, aggregate rate, idle + absolute
/// lifetime. The callback rechecks revocation and shutdown every 100 ms.
fn bridge(
    a: &mut impl BridgeStream,
    b: &mut impl BridgeStream,
    limits: &Limits,
    check: impl Fn() -> bool,
) -> io::Result<()> {
    let start = Instant::now();
    let mut last = start;
    let mut checked = start;
    let mut window = start;
    let mut used = 0_u64;
    let (mut ab, mut ba) = (Buffer::new(), Buffer::new());
    let (mut a_closed, mut b_closed) = (false, false);
    loop {
        if start.elapsed() >= limits.lifetime || last.elapsed() >= limits.idle {
            return Err(bad());
        }
        if checked.elapsed() >= Duration::from_millis(100) {
            if !check() {
                return Err(bad());
            }
            checked = Instant::now();
        }
        if window.elapsed() >= Duration::from_secs(1) {
            used = 0;
            window = Instant::now();
        }
        let n = ab.transfer(a, b, limits.bytes_per_second.saturating_sub(used) as usize)?;
        used += n as u64;
        let m = ba.transfer(b, a, limits.bytes_per_second.saturating_sub(used) as usize)?;
        used += m as u64;
        if ab.eof && ab.start == ab.end && !b_closed {
            match b.close_write() {
                Ok(()) => b_closed = true,
                Err(e) if transient(&e) => {}
                Err(e) => return Err(e),
            }
        }
        if ba.eof && ba.start == ba.end && !a_closed {
            match a.close_write() {
                Ok(()) => a_closed = true,
                Err(e) if transient(&e) => {}
                Err(e) => return Err(e),
            }
        }
        let af = a.flush();
        let bf = b.flush();
        for result in [&af, &bf] {
            if let Err(e) = result
                && !transient(e)
            {
                return Err(bad());
            }
        }
        if n + m > 0 {
            last = Instant::now();
        }
        if (ab.eof && ba.eof)
            && ab.start == ab.end
            && ba.start == ba.end
            && af.is_ok()
            && bf.is_ok()
        {
            return Ok(());
        }
        if n + m == 0 {
            thread::sleep(POLL);
        }
    }
}

/// Four outbound one-shot registration slots. Each incoming stream is piped
/// to the coordinator's own loopback TLS listener; no plaintext application
/// data is available here. Reconnects use capped exponential backoff.
pub fn register(connector: Connector, local: SocketAddr) -> io::Result<RelayHandle> {
    if !local.ip().is_loopback() {
        return Err(bad());
    }
    let shared = Shared::new();
    let flag = shared.clone();
    let join = thread::Builder::new()
        .name("brain-relay-register".into())
        .spawn(move || {
            let mut workers = Vec::new();
            for _ in 0..PER_NAME {
                let (connector, flag) = (connector.clone(), flag.clone());
                if let Ok(worker) = thread::Builder::new()
                    .name("brain-relay-slot".into())
                    .spawn(move || {
                        let mut backoff = Duration::from_millis(100);
                        while !flag.stop.load(Ordering::SeqCst) {
                            let result = (|| -> io::Result<()> {
                                let mut stream = connector.open("register", AUTH_TIMEOUT)?;
                                let _lease = flag.track(&stream.sock, PER_NAME)?;
                                read_signal(
                                    &mut stream,
                                    Instant::now() + Duration::from_secs(35),
                                    b'G',
                                )?;
                                backoff = Duration::from_millis(100);
                                let mut local_stream =
                                    TcpStream::connect_timeout(&local, AUTH_TIMEOUT)?;
                                stream.sock.set_nonblocking(true)?;
                                local_stream.set_nonblocking(true)?;
                                let limits = Limits::default();
                                let result =
                                    bridge(&mut stream, &mut local_stream, &limits, || {
                                        !flag.stop.load(Ordering::SeqCst)
                                    });
                                stream.conn.send_close_notify();
                                let _ = stream.flush();
                                result
                            })();
                            if result.is_err() {
                                let until = Instant::now() + backoff;
                                while Instant::now() < until && !flag.stop.load(Ordering::SeqCst) {
                                    thread::sleep(POLL);
                                }
                                backoff = (backoff * 2).min(Duration::from_secs(5));
                            }
                        }
                    })
                {
                    workers.push(worker);
                }
            }
            for worker in workers {
                let _ = worker.join();
            }
        })?;
    Ok(RelayHandle {
        shared,
        join: Some(join),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Duplex {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
        eof: bool,
        closed: bool,
        partial: bool,
    }
    impl Duplex {
        fn new(bytes: Vec<u8>, eof: bool) -> Self {
            Self {
                input: Cursor::new(bytes),
                output: vec![],
                eof,
                closed: false,
                partial: false,
            }
        }
    }
    impl Read for Duplex {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if self.input.position() == self.input.get_ref().len() as u64 && !self.eof {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.input.read(bytes)
        }
    }
    impl Write for Duplex {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.partial = !self.partial;
            if self.partial {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = bytes.len().min(317);
            self.output.extend_from_slice(&bytes[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl BridgeStream for Duplex {
        fn close_write(&mut self) -> io::Result<()> {
            self.closed = true;
            Ok(())
        }
    }
    #[test]
    fn duplex_rate_backpressure_and_half_close_preserve_every_byte() {
        let payload = (0..4096).map(|n| (n % 251) as u8).collect::<Vec<_>>();
        let mut a = Duplex::new(payload.clone(), true);
        let mut b = Duplex::new(vec![], true);
        let limits = Limits {
            bytes_per_second: 2048,
            idle: Duration::from_secs(3),
            lifetime: Duration::from_secs(5),
            ..Limits::default()
        };
        let start = Instant::now();
        bridge(&mut a, &mut b, &limits, || true).unwrap();
        assert_eq!(b.output, payload);
        assert!(a.closed && b.closed);
        assert!(start.elapsed() >= Duration::from_secs(1));
    }
    #[test]
    fn idle_lifetime_and_revocation_are_bounded() {
        for (idle, lifetime) in [(40, 500), (500, 40)] {
            let (mut a, mut b) = (Duplex::new(vec![], false), Duplex::new(vec![], false));
            let limits = Limits {
                idle: Duration::from_millis(idle),
                lifetime: Duration::from_millis(lifetime),
                ..Limits::default()
            };
            let start = Instant::now();
            assert!(bridge(&mut a, &mut b, &limits, || true).is_err());
            assert!(start.elapsed() < Duration::from_secs(1));
        }
        let (mut a, mut b) = (Duplex::new(vec![], false), Duplex::new(vec![], false));
        let start = Instant::now();
        assert!(bridge(&mut a, &mut b, &Limits::default(), || false).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn connection_slots_and_pending_names_are_released_on_drop() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let shared = Shared::new();
        let lease = shared.track(&socket, 1).unwrap();
        assert!(shared.track(&socket, 1).is_err());
        drop(lease);
        assert!(shared.track(&socket, 1).is_ok());
        let registry: Registry = Arc::default();
        let (tx, _rx) = mpsc::sync_channel(1);
        registry
            .lock()
            .unwrap()
            .insert("alpha".into(), vec![Waiting { id: 1, sender: tx }]);
        drop(PendingRegistration {
            registry: registry.clone(),
            name: "alpha".into(),
            id: 1,
        });
        assert!(registry.lock().unwrap().is_empty());
        shared.cancel();
        assert!(shared.track(&socket, 1).is_err());
    }
    #[test]
    fn socket_deadline_does_not_restart_for_each_tls_read() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let worker = thread::spawn(move || {
            for _ in 0..20 {
                if client.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let start = Instant::now();
        let mut socket = BoundedSocket::new(server, start + Duration::from_millis(60));
        assert!(socket.read_exact(&mut [0; 20]).is_err());
        assert!(start.elapsed() < Duration::from_millis(180));
        drop(socket);
        worker.join().unwrap();
        for target in [
            "localhost:0",
            "host:bad",
            "host/path:123",
            "user@host:443",
            "[::1]:0",
        ] {
            assert!(relay_host(target).is_err());
        }
        assert_eq!(
            relay_host("relay.example.test:7444").unwrap(),
            "relay.example.test"
        );
        assert_eq!(relay_host("[::1]:7444").unwrap(), "::1");
    }
    #[test]
    fn limits_cannot_disable_bounds() {
        for limits in [
            Limits {
                connections: 0,
                ..Limits::default()
            },
            Limits {
                bytes_per_second: 0,
                ..Limits::default()
            },
            Limits {
                idle: Duration::ZERO,
                ..Limits::default()
            },
            Limits {
                lifetime: Duration::from_secs(3601),
                ..Limits::default()
            },
        ] {
            assert!(limits.validate().is_err());
        }
        assert!(Limits::default().validate().is_ok());
        for name in ["ab", "Other", "a/b", "abc\n"] {
            assert!(!valid_name(name));
        }
    }
}
