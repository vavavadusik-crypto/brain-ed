//! The settings window: a loopback-only HTTP server and the page it serves.
//!
//! ADR-001 rejected a desktop toolkit, so the interface is the binary itself:
//! `brain ui` binds 127.0.0.1, serves one self-contained page, and writes the
//! same settings file every other command reads. Nothing is fetched from the
//! network, and the settings page never receives a filesystem path. The graph
//! explorer at `/graph` does show stored node text and source paths — that is
//! its job — but only behind the same per-window token.

use std::error::Error;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::Duration;

use brain_core::{AsOf, SqliteStorage, Storage, StorageStatus};
use clap::{Args, Parser};
use serde_json::{Value, json};

use crate::desktop;
use crate::settings::{
    BUDGET_TOKENS_RANGE, CANDIDATES_RANGE, EmbeddingMode, GRAPH_HOPS_DIMINISHING, GRAPH_HOPS_RANGE,
    MIN_BM25_RANGE, MIN_COSINE_RANGE, QualityOverrides, StoredSettings,
};
use crate::{RuntimeConfig, brain_home, settings};

/// The page, inlined so the binary has no runtime asset directory to lose.
const PAGE: &str = include_str!("../assets/settings.html");

/// The graph view. The renderer is the one the owner already used and trusted;
/// this binary carries it so the window has no asset directory to lose.
const GRAPH_PAGE: &str = include_str!("../assets/viewer/index.html");

/// The explorer: every stored node reachable by search, filter and paging,
/// with a small neighbourhood drawn on demand instead of the whole graph.
const EXPLORER_PAGE: &str = include_str!("../assets/explorer.html");

/// The project work map: projects, their tasks and history, read-only.
const MAP_PAGE: &str = include_str!("../assets/map.html");

/// The genuine 3D renderer of the work map: deterministic XYZ layout, one
/// perspective projection, depth ordering, zoom/reset and mouse/touch picking.
/// A static asset carried by the binary so `/map/map3d.js` needs no directory.
const MAP3D_SCRIPT: &str = include_str!("../assets/map3d.js");

/// Revision of the routes this binary serves. A window answering the ping
/// with an older number (or none) lacks a route this page uses (revision 5:
/// `/api/work/board`), so the launcher replaces it instead of sending someone
/// to a page whose panel would fail.
///
/// Revision 7: pages no longer carry the window key (it reaches the tab via
/// the launcher's URL fragment) and `brain-data.json` needs the key, so an
/// older window — whose HTML hands its key to any local reader — is replaced.
const API_REVISION: u64 = 7;
const VIEWER_APP: &str = include_str!("../assets/viewer/app.js");
const GRAPH3D_SCRIPT: &str = include_str!("../assets/graph3d.js");
const VIEWER_STYLE: &str = include_str!("../assets/viewer/style.css");
const VIEWER_THREE: &str = include_str!("../assets/viewer/three.min.js");

/// Marker older pages carried for the server to stamp its key into. No page
/// may contain it now: the key is never part of served HTML.
#[cfg(test)]
const TOKEN_PLACEHOLDER: &str = "__BRAIN_UI_TOKEN__";

/// Ceiling on a request head; anything larger is a client bug or an attack.
/// Sized for a percent-encoded node id of up to 4 KiB of Cyrillic.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Ceiling on a request body. The settings document is a few hundred bytes.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// How long one connection may stay silent before it is dropped.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// Connections served at once. Browsers open a handful; the rest wait.
const MAX_CONCURRENT_CONNECTIONS: usize = 16;

/// Marker one running window uses to recognise another on the same port.
const SERVICE_MARKER: &str = "brain-settings";

/// The CLI, MCP server, and background worker all honour `embedding_mode`.
const EMBEDDING_RUNTIME_IMPLEMENTED: bool = true;

/// Headers sent with every response.
///
/// `default-src 'none'` plus an explicit allowance for the inline style and
/// script is what makes "no external requests" a property of the page rather
/// than a promise about its source.
const SECURITY_HEADERS: &str = concat!(
    "X-Content-Type-Options: nosniff\r\n",
    "Referrer-Policy: no-referrer\r\n",
    "Cache-Control: no-store\r\n",
    "Content-Security-Policy: default-src 'none'; script-src 'self' 'unsafe-inline'; ",
    "style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; ",
    "form-action 'none'; frame-ancestors 'none'\r\n",
);

#[derive(Clone, Debug, Args)]
pub(crate) struct UiArgs {
    /// Port on 127.0.0.1; zero asks the operating system for a free one.
    #[arg(long, default_value_t = 0)]
    port: u16,

    /// Print the address and wait instead of opening a browser.
    #[arg(long)]
    no_open: bool,

    /// Write the desktop shortcut and exit without serving anything.
    #[arg(long)]
    install_shortcut: bool,

    /// Remove the desktop shortcut and exit without serving anything.
    #[arg(long, conflicts_with = "install_shortcut")]
    remove_shortcut: bool,

    /// With --install-shortcut or --remove-shortcut: print the paths that
    /// would change and change nothing.
    #[arg(long)]
    dry_run: bool,

    /// Which page to open: settings, or the graph of what is stored.
    #[arg(long, default_value = "settings", value_parser = ["settings", "graph", "graph3d"])]
    page: String,
}

impl UiArgs {
    /// Path the browser is sent to.
    fn path(&self) -> &'static str {
        match self.page.as_str() {
            "graph" => "/graph",
            "graph3d" => "/graph/3d",
            _ => "/",
        }
    }
}

/// Immutable facts every connection thread needs.
struct Server {
    space: String,
    brain_home: PathBuf,
    database: PathBuf,
    token: String,
    /// Public, random per-process tag that the ping reports; ties a private
    /// owner file to exactly one running window.
    generation: String,
    address: SocketAddr,
}

impl Server {
    /// The only two authorities a browser may use to reach us.
    ///
    /// Checking this rejects DNS rebinding, where a hostile name resolves to
    /// 127.0.0.1 so a remote page can address a local server.
    fn accepts_host(&self, host: &str) -> bool {
        let port = self.address.port();
        host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
    }

    /// The only two origins allowed to submit a change.
    fn accepts_origin(&self, origin: &str) -> bool {
        let port = self.address.port();
        origin == format!("http://127.0.0.1:{port}") || origin == format!("http://localhost:{port}")
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.address.port())
    }
}

/// Address that hands the window key to a browser tab: the key rides in the
/// fragment, which browsers never send to a server or put in a Referer.
fn owner_url(port: u16, path: &str, token: &str) -> String {
    format!("http://127.0.0.1:{port}{path}#k={token}")
}

/// Private page the browser is launched on: the launch argv (readable by
/// other local accounts through /proc on most Linux systems) carries only this
/// file's path, and the page forwards the tab to the keyed fragment URL.
fn bootstrap_file(brain_home: &std::path::Path, port: u16) -> PathBuf {
    brain_home.join("ui").join(format!("open-{port}.html"))
}

/// The bootstrap page for `path` on `port`. Only digits, hex and the fixed
/// page paths reach it, so nothing in it needs escaping.
fn bootstrap_page(port: u16, path: &str, token: &str) -> String {
    let target = owner_url(port, path, token);
    format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<meta name=\"referrer\" content=\"no-referrer\">\n\
         <meta http-equiv=\"refresh\" content=\"0;url={target}\">\n<title>Brain ED</title>\n\
         <script>location.replace(\"{target}\");</script>\n<a href=\"{target}\">Brain ED</a>\n"
    )
}

/// Writes the bootstrap page owner-only and opens the browser on its path.
/// Refuses (and opens nothing) unless `ui` is a real directory of this user
/// that no one else can enter.
fn open_owner(
    brain_home: &std::path::Path,
    port: u16,
    path: &str,
    token: &str,
) -> Result<(), Box<dyn Error>> {
    let file = bootstrap_file(brain_home, port);
    let directory = file.parent().ok_or("no ui directory")?;
    crate::private_fs::ensure_private_home(brain_home)?;
    crate::private_fs::ensure_private_home(directory)?;
    let metadata = std::fs::symlink_metadata(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !metadata.file_type().is_dir()
            || crate::private_fs::current_uid() != Some(metadata.uid())
            || metadata.mode() & 0o077 != 0
        {
            return Err(format!(
                "{} не приватный каталог; браузер не открыт",
                directory.display()
            )
            .into());
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    crate::private_fs::write_private(&file, bootstrap_page(port, path, token).as_bytes())?;
    open_browser(file.to_str().ok_or("bootstrap path is not UTF-8")?)?;
    Ok(())
}

/// Private file through which a later launcher on the same fixed port, or the
/// owner after `--no-open`, recovers the running window's key.
fn owner_file(brain_home: &std::path::Path, port: u16) -> PathBuf {
    brain_home.join("ui").join(format!("owner-{port}.json"))
}

/// Writes the owner file 0600 inside an owner-only `ui` directory.
fn write_owner_file(server: &Server) -> Result<PathBuf, Box<dyn Error>> {
    let path = owner_file(&server.brain_home, server.address.port());
    crate::private_fs::ensure_private_home(&server.brain_home)?;
    crate::private_fs::ensure_private_home(path.parent().ok_or("no ui directory")?)?;
    let document = json!({
        "service": SERVICE_MARKER,
        "api": API_REVISION,
        "pid": std::process::id(),
        "port": server.address.port(),
        "generation": server.generation,
        "token": server.token,
        "url": owner_url(server.address.port(), "/", &server.token),
    });
    crate::private_fs::write_private(&path, &serde_json::to_vec_pretty(&document)?)?;
    Ok(path)
}

/// Reads the key of the window answering on `port`, failing closed unless the
/// owner file is a private regular file of this user whose pid and generation
/// match what the window itself reports, and (on Linux) whose pid is the
/// process actually holding the listening socket.
fn recover_owner_token(
    brain_home: &std::path::Path,
    port: u16,
    pid: Option<u32>,
    generation: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let path = owner_file(brain_home, port);
    let refuse = |why: &str| -> Box<dyn Error> {
        format!(
            "окно на порту {port} не удалось подтвердить ({why}); ключ не выдан — закройте его и запустите снова"
        )
        .into()
    };
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| refuse("нет файла владельца"))?;
    if !metadata.file_type().is_file() {
        return Err(refuse("файл владельца не обычный файл"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if crate::private_fs::current_uid() != Some(metadata.uid()) || metadata.mode() & 0o077 != 0
        {
            return Err(refuse("файл владельца не приватный"));
        }
    }
    let mut text = String::new();
    std::fs::File::open(&path)?
        .take(16 * 1024)
        .read_to_string(&mut text)?;
    let stored: Value =
        serde_json::from_str(&text).map_err(|_| refuse("файл владельца повреждён"))?;
    let (Some(pid), Some(generation)) = (pid, generation) else {
        return Err(refuse("окно не сообщило pid"));
    };
    if stored["service"] != SERVICE_MARKER
        || stored["port"].as_u64() != Some(u64::from(port))
        || stored["pid"].as_u64() != Some(u64::from(pid))
        || stored["generation"].as_str() != Some(generation)
    {
        return Err(refuse("файл владельца от другого окна"));
    }
    #[cfg(target_os = "linux")]
    if listening_pid(port)? != Some(pid) {
        return Err(refuse("порт держит другой процесс"));
    }
    let token = stored["token"].as_str().unwrap_or_default();
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(refuse("ключ в файле владельца неверен"));
    }
    Ok(token.to_owned())
}

pub(crate) fn run(config: &RuntimeConfig, args: &UiArgs) -> Result<(), Box<dyn Error>> {
    if args.dry_run && !args.install_shortcut && !args.remove_shortcut {
        return Err("--dry-run applies only to --install-shortcut or --remove-shortcut".into());
    }
    if args.install_shortcut {
        return desktop::install(args.dry_run);
    }
    if args.remove_shortcut {
        return desktop::remove(args.dry_run);
    }

    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, args.port)) {
        Ok(listener) => listener,
        // A fixed port is what the desktop shortcuts ask for, so a second click
        // must reuse the window that is already open rather than fail — unless
        // that window is an older binary that lacks the page being asked for.
        Err(error) if error.kind() == io::ErrorKind::AddrInUse && args.port != 0 => {
            match take_over_or_reuse(args)? {
                Some(listener) => listener,
                None => return Ok(()),
            }
        }
        Err(error) => return Err(error.into()),
    };
    let server = Arc::new(Server {
        space: config.space.clone(),
        brain_home: brain_home(),
        database: config.database.clone(),
        token: fresh_token()?,
        generation: fresh_token()?[..32].to_owned(),
        address: listener.local_addr()?,
    });
    let status = StatusService::spawn(config.database.clone());
    let owner = write_owner_file(&server)?;

    // The key is never printed: stdout is logged by launchers and terminals.
    // Whoever owns the home directory reads it from the private owner file.
    crate::print_json(&json!({
        "status": "listening",
        "url": server.url(),
        "port": server.address.port(),
        "interface": "127.0.0.1 only",
        "settings_file": settings::settings_path(&server.brain_home),
        "owner_file": owner,
        "hint": "закройте окно и нажмите Ctrl+C, чтобы остановить сервер; без браузера откройте адрес \"url\" из файла владельца",
    }))?;

    if !args.no_open
        && let Err(error) = open_owner(
            &server.brain_home,
            server.address.port(),
            args.path(),
            &server.token,
        )
    {
        eprintln!("warning: не удалось открыть браузер ({error}); откройте адрес вручную");
    }

    serve(&listener, &server, &status);
    Ok(())
}

/// Who answers on a port someone else already bound.
#[derive(Debug, PartialEq, Eq)]
enum Occupant {
    /// Our window, serving every route this binary knows: its self-reported
    /// pid and generation, checked against the private owner file.
    Current {
        pid: Option<u32>,
        generation: Option<String>,
    },
    /// Our window, but an older binary: its pid when it said so.
    Stale(Option<u32>),
    /// Something else entirely, or nothing that answers.
    Stranger,
}

/// Reads the occupant from a ping answer.
fn classify(answer: &str) -> Occupant {
    let Some((_, body)) = answer.split_once("\r\n\r\n") else {
        return Occupant::Stranger;
    };
    let Ok(ping) = serde_json::from_str::<Value>(body.trim()) else {
        return Occupant::Stranger;
    };
    if ping["service"] != SERVICE_MARKER {
        return Occupant::Stranger;
    }
    match ping["api"].as_u64() {
        Some(revision) if revision >= API_REVISION => Occupant::Current {
            pid: ping["pid"].as_u64().and_then(|pid| u32::try_from(pid).ok()),
            generation: ping["generation"].as_str().map(str::to_owned),
        },
        _ => Occupant::Stale(ping["pid"].as_u64().and_then(|pid| u32::try_from(pid).ok())),
    }
}

/// Reuses a current window, replaces a stale one, refuses a stranger.
///
/// Returns the listener when this process took the port over and should
/// serve, and `None` when an existing window was pointed at instead. The
/// probe is retried briefly: two clicks in quick succession can find the
/// port bound by a window that has not started answering yet.
fn take_over_or_reuse(args: &UiArgs) -> Result<Option<TcpListener>, Box<dyn Error>> {
    let mut occupant = Occupant::Stranger;
    for attempt in 0..5 {
        occupant = probe(args.port);
        if occupant != Occupant::Stranger {
            break;
        }
        thread::sleep(Duration::from_millis(200 * (attempt + 1)));
    }
    let url = format!("http://127.0.0.1:{}{}", args.port, args.path());
    match occupant {
        Occupant::Stranger => Err(format!(
            "порт {} занят другой программой; укажите другой через --port",
            args.port
        )
        .into()),
        Occupant::Current { pid, generation } => {
            let token = recover_owner_token(&brain_home(), args.port, pid, generation.as_deref())?;
            crate::print_json(&json!({
                "status": "already_listening",
                "url": url,
                "port": args.port,
                "owner_file": owner_file(&brain_home(), args.port),
                "hint": "окно уже открыто на этом порту",
            }))?;
            if !args.no_open
                && let Err(error) = open_owner(&brain_home(), args.port, args.path(), &token)
            {
                eprintln!("warning: не удалось открыть браузер ({error}); откройте адрес вручную");
            }
            Ok(None)
        }
        Occupant::Stale(_) => {
            let pid = stop_stale_window(args.port)?;
            for _ in 0..50 {
                match TcpListener::bind((Ipv4Addr::LOCALHOST, args.port)) {
                    Ok(listener) => {
                        eprintln!(
                            "brain ui: остановлено устаревшее окно (pid {pid}) на порту {}; открытые вкладки нужно обновить",
                            args.port
                        );
                        return Ok(Some(listener));
                    }
                    Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            // Another click may have won the same race and already serves the
            // port; pointing at it beats failing.
            if let Occupant::Current { pid, generation } = probe(args.port) {
                let token =
                    recover_owner_token(&brain_home(), args.port, pid, generation.as_deref())?;
                if !args.no_open {
                    let _ = open_owner(&brain_home(), args.port, args.path(), &token);
                }
                return Ok(None);
            }
            Err(format!(
                "устаревшее окно (pid {pid}) не освободило порт {}",
                args.port
            )
            .into())
        }
    }
}

/// Asks whoever holds the port who they are.
fn probe(port: u16) -> Occupant {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, CONNECTION_TIMEOUT) else {
        return Occupant::Stranger;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let request =
        format!("GET /api/ping HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return Occupant::Stranger;
    }
    let mut answer = Vec::new();
    let _ = stream.take(4_096).read_to_end(&mut answer);
    classify(&String::from_utf8_lossy(&answer))
}

/// Stops exactly the old `brain ui` process listening on `port`.
///
/// Old windows do not report their pid, so the owner is found the way `ss`
/// finds it: the listening socket's inode in /proc/net/tcp, then the process
/// holding that inode. It is stopped only if it is a `brain` binary running
/// `ui`; anything else is left alone and reported.
#[cfg(target_os = "linux")]
fn stop_stale_window(port: u16) -> Result<u32, Box<dyn Error>> {
    stop_listener(port, true)
}

/// The pid of this user's process listening on 127.0.0.1:`port`, if any.
#[cfg(target_os = "linux")]
fn listening_pid(port: u16) -> Result<Option<u32>, Box<dyn Error>> {
    match stop_listener(port, false) {
        Ok(pid) => Ok(Some(pid)),
        Err(_) => Ok(None),
    }
}

/// Finds (and, with `stop`, terminates) the `brain ui` listening on `port`.
#[cfg(target_os = "linux")]
fn stop_listener(port: u16, stop: bool) -> Result<u32, Box<dyn Error>> {
    let table = std::fs::read_to_string("/proc/net/tcp")?;
    let wanted = format!("0100007F:{port:04X}");
    let inode = table
        .lines()
        .skip(1)
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find(|fields| fields.len() > 9 && fields[1] == wanted && fields[3] == "0A")
        .map(|fields| fields[9].to_owned())
        .ok_or("не найден слушающий сокет устаревшего окна")?;
    let marker = format!("socket:[{inode}]");
    let holds_socket = |pid_dir: &std::path::Path| {
        std::fs::read_dir(pid_dir.join("fd")).is_ok_and(|descriptors| {
            descriptors.flatten().any(|fd| {
                std::fs::read_link(fd.path())
                    .is_ok_and(|target| target.as_os_str() == marker.as_str())
            })
        })
    };
    for entry in std::fs::read_dir("/proc")?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        // Only our own processes expose their descriptors, so another
        // user's window is never a candidate.
        if !holds_socket(&entry.path()) {
            continue;
        }
        let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let argv = cmdline
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect::<Vec<_>>();
        let is_brain = argv
            .first()
            .and_then(|program| std::path::Path::new(program).file_name())
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("brain"));
        let is_ui = crate::Cli::try_parse_from(&argv)
            .is_ok_and(|cli| matches!(cli.command, crate::Command::Ui(_)));
        if !is_brain || !is_ui {
            return Err(format!(
                "порт {port} держит процесс {pid}, который не является окном brain ui"
            )
            .into());
        }
        // Checked again just before signalling, so the window in which the
        // pid could be recycled is the length of one syscall, not of the scan.
        if !stop {
            return Ok(pid);
        }
        if !holds_socket(&entry.path()) {
            return Err("устаревшее окно завершилось само; запустите ещё раз".into());
        }
        let status = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()?;
        if !status.success() {
            return Err(format!("не удалось остановить устаревшее окно (pid {pid})").into());
        }
        return Ok(pid);
    }
    Err("не найден процесс устаревшего окна".into())
}

#[cfg(not(target_os = "linux"))]
fn stop_stale_window(port: u16) -> Result<u32, Box<dyn Error>> {
    Err(
        format!("на порту {port} открыто окно старой версии; закройте его и запустите снова")
            .into(),
    )
}

/// Accepts connections until the process is stopped.
///
/// One thread per connection, capped: a browser opens several sockets at once
/// and a speculative one that never sends a byte must not stall the others.
fn serve(listener: &TcpListener, server: &Arc<Server>, status: &StatusService) {
    let live = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        if live.load(Ordering::SeqCst) >= MAX_CONCURRENT_CONNECTIONS {
            drop(stream);
            continue;
        }
        live.fetch_add(1, Ordering::SeqCst);
        let server = Arc::clone(server);
        let counter = Arc::clone(&live);
        let requester = status.requester();
        let spawned = thread::Builder::new()
            .name("brain-ui-connection".to_owned())
            .spawn(move || {
                handle_connection(stream, &server, &requester);
                counter.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            live.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn handle_connection(mut stream: TcpStream, server: &Server, status: &StatusRequester) {
    let _ = stream.set_read_timeout(Some(CONNECTION_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CONNECTION_TIMEOUT));
    let response = match read_request(&mut stream) {
        Ok(request) => route(server, status, &request),
        Err(response) => response,
    };
    let _ = write_response(&mut stream, &response);
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

struct Response {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

/// Reads one request, refusing anything oversized or malformed.
///
/// Keep-alive is not offered: every response closes the connection, so a single
/// read of head plus declared body is the whole protocol we need.
fn read_request(stream: &mut TcpStream) -> Result<Request, Response> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    let head_end = loop {
        if let Some(position) = find_marker(&buffer, b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(problem(
                431,
                "Request Header Fields Too Large",
                "заголовки запроса слишком велики",
            ));
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(problem(
                    400,
                    "Bad Request",
                    "запрос оборвался до конца заголовков",
                ));
            }
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(_) => {
                return Err(problem(
                    408,
                    "Request Timeout",
                    "запрос не уложился в таймаут",
                ));
            }
        }
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_ascii_uppercase();
    let target = request_line.next().unwrap_or_default().to_owned();
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let (path, query) = (path.to_owned(), query.to_owned());

    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let request = Request {
        method,
        path,
        query,
        headers,
        body: Vec::new(),
    };

    // Chunked bodies would need a second parser for no gain: the page only ever
    // posts one small JSON document with a known length.
    if request.header("transfer-encoding").is_some() {
        return Err(problem(
            411,
            "Length Required",
            "поддерживается только тело с Content-Length",
        ));
    }
    let declared = match request.header("content-length") {
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| problem(400, "Bad Request", "некорректный Content-Length"))?,
        None => 0,
    };
    if declared > MAX_BODY_BYTES {
        return Err(problem(
            413,
            "Payload Too Large",
            "тело запроса слишком велико",
        ));
    }

    let mut body = buffer[head_end + 4..].to_vec();
    body.truncate(declared);
    while body.len() < declared {
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(problem(
                    400,
                    "Bad Request",
                    "тело запроса короче объявленного",
                ));
            }
            Ok(read) => {
                let wanted = declared - body.len();
                body.extend_from_slice(&chunk[..read.min(wanted)]);
            }
            Err(_) => {
                return Err(problem(
                    408,
                    "Request Timeout",
                    "тело запроса не уложилось в таймаут",
                ));
            }
        }
    }
    Ok(Request { body, ..request })
}

/// Maps one request to one response, after the origin checks.
///
/// The key is what separates the owner's tab from any other page or local
/// account: it is never in served HTML, only in the launcher's URL fragment
/// and the owner-only owner file, and without it no data route answers.
fn route(server: &Server, status: &StatusRequester, request: &Request) -> Response {
    let host = request.header("host").unwrap_or_default();
    if !server.accepts_host(host) {
        return problem(
            403,
            "Forbidden",
            "окно настроек отвечает только на 127.0.0.1",
        );
    }
    let authorized = request
        .header("x-brain-token")
        .is_some_and(|token| tokens_match(token, &server.token));
    let origin_allowed = request
        .header("origin")
        .is_none_or(|origin| server.accepts_origin(origin));

    match (request.method.as_str(), request.path.as_str()) {
        // Deliberately unauthenticated and deliberately empty of state: it
        // exists so a second `brain ui` can tell our window from a stranger
        // holding the same port, and it says nothing a port scan would not.
        ("GET", "/api/ping") => json_response(
            200,
            "OK",
            &json!({"status": "ok", "service": SERVICE_MARKER, "api": API_REVISION,
                    "pid": std::process::id(), "generation": server.generation}),
        ),
        ("GET", "/") => Response {
            status: 200,
            reason: "OK",
            content_type: "text/html; charset=utf-8",
            body: PAGE.as_bytes().to_vec(),
        },
        // The same mark the desktop shortcut installs, so the browser tab and
        // the launcher entry look like one thing.
        ("GET", "/icon.svg") => Response {
            status: 200,
            reason: "OK",
            content_type: "image/svg+xml",
            body: desktop::ICON_SVG.as_bytes().to_vec(),
        },
        // The old address and desktop entries keep working: they open the
        // explorer, whose graph opens in 3D by default.
        ("GET", "/graph" | "/graph/3d") => Response {
            status: 200,
            reason: "OK",
            content_type: "text/html; charset=utf-8",
            body: EXPLORER_PAGE.as_bytes().to_vec(),
        },
        ("GET", "/graph/graph3d.js") => Response {
            status: 200,
            reason: "OK",
            content_type: "text/javascript; charset=utf-8",
            body: GRAPH3D_SCRIPT.as_bytes().to_vec(),
        },
        // The previous viewer, kept reachable by its own name. Its page loads
        // assets by relative path, which from under /graph/ meant three 404s
        // and a page stuck on "loading"; the aliases below serve them there.
        ("GET", "/graph/legacy-3d") => Response {
            status: 200,
            reason: "OK",
            content_type: "text/html; charset=utf-8",
            body: GRAPH_PAGE.as_bytes().to_vec(),
        },
        ("GET", "/api/state") => {
            if !authorized {
                return problem(403, "Forbidden", "нет ключа окна настроек");
            }
            json_response(200, "OK", &state_document(server, status))
        }
        ("GET", "/brain-viewer/app.js" | "/graph/brain-viewer/app.js") => Response {
            status: 200,
            reason: "OK",
            content_type: "text/javascript; charset=utf-8",
            body: VIEWER_APP.as_bytes().to_vec(),
        },
        ("GET", "/brain-viewer/style.css" | "/graph/brain-viewer/style.css") => Response {
            status: 200,
            reason: "OK",
            content_type: "text/css; charset=utf-8",
            body: VIEWER_STYLE.as_bytes().to_vec(),
        },
        ("GET", "/vendor/three.min.js" | "/graph/vendor/three.min.js") => Response {
            status: 200,
            reason: "OK",
            content_type: "text/javascript; charset=utf-8",
            body: VIEWER_THREE.as_bytes().to_vec(),
        },
        // Stored node text, so the same key as every other data route: being
        // bound to loopback does not keep other local users or pages out.
        ("GET", "/brain-data.json") | ("GET", "/graph/brain-data.json") => {
            if !authorized {
                return problem(403, "Forbidden", "нет ключа окна настроек");
            }
            match crate::graph_view::document(
                &server.database,
                &server.space,
                crate::graph_view::DEFAULT_LIMIT,
            ) {
                Ok(document) => json_response(200, "OK", &document),
                Err(error) => json_response(
                    200,
                    "OK",
                    &json!({"format": "brain-data-v2", "nodes": [], "links": [],
                            "error": error.to_string()}),
                ),
            }
        }
        ("GET", "/api/graph") => {
            if !authorized {
                return problem(403, "Forbidden", "нет ключа окна настроек");
            }
            match crate::graph_view::document(
                &server.database,
                &server.space,
                crate::graph_view::DEFAULT_LIMIT,
            ) {
                Ok(document) => json_response(200, "OK", &document),
                // The page draws an empty canvas either way; saying why beats
                // leaving someone to wonder whether their memory is empty.
                Err(error) => json_response(
                    200,
                    "OK",
                    &json!({"available": false, "error": error.to_string()}),
                ),
            }
        }
        (
            "GET",
            "/api/explore/overview"
            | "/api/explore/nodes"
            | "/api/explore/node"
            | "/api/explore/clusters"
            | "/api/explore/graph",
        ) => {
            if !authorized {
                return problem(403, "Forbidden", "нет ключа окна настроек");
            }
            explore_response(server, request)
        }
        // A read, but its argument is a list of up to a thousand long ids, which
        // does not fit a URL; POST carries it, with the same origin check as
        // every other POST.
        ("POST", "/api/explore/links") => {
            if !authorized || !origin_allowed {
                return problem(403, "Forbidden", "нет ключа окна настроек");
            }
            explore_response(server, request)
        }
        ("GET", "/api/archive/status" | "/api/archive/search" | "/api/archive/read") => {
            if !authorized {
                return problem(403, "Forbidden", "нет ключа окна настроек");
            }
            archive_response(server, request)
        }
        ("GET", "/api/archive/backfill") => {
            if !authorized {
                return problem(403, "Forbidden", "нет ключа окна настроек");
            }
            json_response(
                200,
                "OK",
                &crate::archive::backfill_document(&server.brain_home),
            )
        }
        // An explicit user action (start/stop historic backfill). Same origin and
        // token checks as every other POST; the body is a strict bounded object and
        // an invalid or unauthenticated request never writes any config.
        ("POST", "/api/archive/backfill") => {
            if !authorized || !origin_allowed {
                return problem(403, "Forbidden", "нет ключа окна настроек");
            }
            archive_backfill_action(server, &request.body)
        }
        ("GET", "/api/work/board") => {
            if !authorized {
                return problem(403, "Forbidden", "нет ключа окна настроек");
            }
            work_response(server, request)
        }
        ("GET", "/map") => Response {
            status: 200,
            reason: "OK",
            content_type: "text/html; charset=utf-8",
            body: MAP_PAGE.as_bytes().to_vec(),
        },
        // The map's 3D renderer, requested by the page like the graph's script:
        // a static asset with no state, so no key is needed to fetch it.
        ("GET", "/map/map3d.js") => Response {
            status: 200,
            reason: "OK",
            content_type: "text/javascript; charset=utf-8",
            body: MAP3D_SCRIPT.as_bytes().to_vec(),
        },
        ("GET", path) if path.starts_with("/api/map/") => {
            if !authorized {
                return problem(403, "Forbidden", "нет ключа окна настроек");
            }
            let params = crate::explore::parse_query(&request.query);
            let (code, value) =
                crate::map::ui_get(&server.brain_home, &server.space, path, &params);
            map_json(code, &value)
        }
        // Creating the map and reading coordinator events into it are the
        // user's explicit actions in this window: key and a same-origin Origin
        // are both required (an absent Origin is refused here).
        ("POST", "/api/map/init" | "/api/map/ingest") => {
            let same_origin = request
                .header("origin")
                .is_some_and(|origin| server.accepts_origin(origin));
            if !authorized || !same_origin {
                return problem(
                    403,
                    "Forbidden",
                    "нет ключа окна настроек или чужой источник",
                );
            }
            let params = crate::explore::parse_query(&request.query);
            let (code, value) =
                crate::map::ui_post(&server.brain_home, &server.space, &request.path, &params);
            map_json(code, &value)
        }
        ("POST", "/api/settings") => {
            if !authorized || !origin_allowed {
                return problem(403, "Forbidden", "нет ключа окна настроек");
            }
            apply(server, status, &request.body)
        }
        ("GET" | "POST", _) => problem(404, "Not Found", "такого адреса нет"),
        _ => problem(405, "Method Not Allowed", "метод не поддерживается"),
    }
}

fn map_json(code: u16, value: &Value) -> Response {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Service Unavailable",
    };
    json_response(code, reason, value)
}

/// Read-only workboard: one page of tasks plus live claims of the window's
/// space. Opens the coordinator read-only; nothing here can claim or change.
fn work_response(server: &Server, request: &Request) -> Response {
    use brain_core::coordination::{Coordinator, ErrorKind, database_path};
    let params = crate::explore::parse_query(&request.query);
    let text = |key: &str| params.get(key).filter(|value| !value.is_empty()).cloned();
    let number = |key: &str, default: usize| -> Result<usize, String> {
        text(key).map_or(Ok(default), |value| {
            value
                .parse::<usize>()
                .map_err(|_| format!("{key} должно быть целым числом"))
        })
    };
    let (offset, limit) = match (number("offset", 0), number("limit", 25)) {
        (Ok(offset), Ok(limit)) => (offset, limit),
        (Err(message), _) | (_, Err(message)) => return problem(400, "Bad Request", &message),
    };
    let project = text("project");
    let state = text("state");
    let result =
        Coordinator::open_read_only(&database_path(&server.brain_home)).and_then(|coordinator| {
            let mut board = coordinator.board(
                &server.space,
                project.as_deref(),
                state.as_deref(),
                false,
                offset,
                limit,
            )?;
            board["claims"] =
                coordinator.claims(&server.space, project.as_deref(), 0, 50)?["claims"].take();
            Ok(board)
        });
    match result {
        Ok(document) => json_response(200, "OK", &document),
        Err(error) if error.kind == ErrorKind::NotConfigured => json_response(
            200,
            "OK",
            &serde_json::json!({"status": "not_configured", "hint": error.message, "tasks": [], "claims": []}),
        ),
        Err(error) if error.kind == ErrorKind::InvalidInput => {
            problem(400, "Bad Request", &error.message)
        }
        Err(error) => problem(503, "Service Unavailable", &error.to_string()),
    }
}

/// Answers one explorer request, mapping each failure to its HTTP meaning.
fn explore_response(server: &Server, request: &Request) -> Response {
    let params = crate::explore::parse_query(&request.query);
    // Each request names one stored space or gets the window's own; spaces are
    // never merged, so a node from one cannot surface while browsing another.
    let result =
        crate::explore::resolve_space(&server.database, &params, &server.space).and_then(|space| {
            match request.path.as_str() {
                "/api/explore/overview" => {
                    crate::explore::overview(&server.database, &space).map(|mut overview| {
                        if let Some(object) = overview.as_object_mut() {
                            object.insert("work".to_owned(), work_document(server, &space));
                        }
                        overview
                    })
                }
                "/api/explore/nodes" => crate::explore::nodes(&server.database, &space, &params),
                "/api/explore/clusters" => {
                    crate::explore::clusters(&server.database, &space, &params)
                }
                "/api/explore/graph" => crate::explore::graph(&server.database, &space, &params),
                "/api/explore/links" => {
                    crate::explore::links(&server.database, &space, &request.body)
                }
                _ => crate::explore::node(&server.database, &space, &params),
            }
        });
    match result {
        Ok(document) => json_response(200, "OK", &document),
        Err(crate::explore::ExploreError::BadRequest(message)) => {
            problem(400, "Bad Request", &message)
        }
        Err(crate::explore::ExploreError::NotFound(message)) => problem(404, "Not Found", &message),
        Err(crate::explore::ExploreError::Database(error)) => problem(
            503,
            "Service Unavailable",
            &format!("база памяти недоступна: {error}"),
        ),
    }
}

/// The conversation archive, read-only. Text in the answers is untrusted and
/// the page renders it with textContent only.
fn archive_response(server: &Server, request: &Request) -> Response {
    use crate::archive::{self, ArchiveError};
    let params = crate::explore::parse_query(&request.query);
    let number = |key: &str| -> Result<Option<usize>, ArchiveError> {
        params
            .get(key)
            .filter(|value| !value.is_empty())
            .map(|value| {
                value.parse::<usize>().map_err(|_| {
                    ArchiveError::BadRequest(format!("{key} должно быть целым числом"))
                })
            })
            .transpose()
    };
    let text = |key: &str| params.get(key).filter(|value| !value.is_empty()).cloned();
    let result = match request.path.as_str() {
        "/api/archive/status" => Ok(archive::status(&server.brain_home, &crate::user_home())),
        "/api/archive/search" => number("limit").and_then(|limit| {
            archive::search(
                &server.brain_home,
                &archive::SearchRequest {
                    query: text("q").unwrap_or_default(),
                    client: text("client"),
                    role: text("role"),
                    session_id: text("session"),
                    project_root: text("project_root"),
                    limit,
                    budget_tokens: number("budget_tokens")?,
                },
            )
        }),
        _ => number("offset").and_then(|offset| {
            archive::read(
                &server.brain_home,
                &archive::ReadRequest {
                    id: text("id").unwrap_or_default(),
                    offset,
                    max_bytes: number("max_bytes")?,
                    context: number("context")?,
                },
            )
        }),
    };
    match result {
        Ok(document) => json_response(200, "OK", &document),
        Err(ArchiveError::BadRequest(message)) => problem(400, "Bad Request", &message),
        Err(ArchiveError::NotFound(message)) => problem(404, "Not Found", &message),
        Err(error) => problem(503, "Service Unavailable", &error.to_string()),
    }
}

/// Handles an explicit backfill start/stop POST with a strict, bounded body.
///
/// Only an authenticated, same-origin request reaches here. The body must be a
/// JSON object with `action` of `start` or `stop` and, for `start`, an integer
/// `files_per_run` in 1..=20; any extra key or wrong type is refused before
/// anything is written, so an invalid request never touches the config.
fn archive_backfill_action(server: &Server, body: &[u8]) -> Response {
    let payload = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => map,
        Ok(_) => return problem(400, "Bad Request", "тело запроса должно быть объектом JSON"),
        Err(_) => return problem(400, "Bad Request", "тело запроса не является JSON"),
    };
    for key in payload.keys() {
        if !matches!(key.as_str(), "action" | "files_per_run") {
            return problem(400, "Bad Request", &format!("неизвестное поле: {key}"));
        }
    }

    match payload.get("action").and_then(Value::as_str) {
        Some("start") => {
            let files = match payload.get("files_per_run") {
                Some(Value::Number(number)) => number.as_u64(),
                _ => None,
            };
            match files {
                Some(files @ 1..=20) => crate::archive::backfill_start(&server.brain_home, files)
                    .map(|document| json_response(200, "OK", &document))
                    .unwrap_or_else(|error| problem(400, "Bad Request", &error.to_string())),
                _ => problem(
                    400,
                    "Bad Request",
                    "files_per_run должно быть целым числом 1..20",
                ),
            }
        }
        Some("stop") => crate::archive::backfill_stop(&server.brain_home)
            .map(|document| json_response(200, "OK", &document))
            .unwrap_or_else(|error| problem(500, "Internal Server Error", &error.to_string())),
        _ => problem(400, "Bad Request", "action должно быть start или stop"),
    }
}

/// Validates and persists a submitted settings document.
fn apply(server: &Server, status: &StatusRequester, body: &[u8]) -> Response {
    let Ok(payload) = serde_json::from_slice::<Value>(body) else {
        return problem(400, "Bad Request", "тело запроса не является JSON");
    };
    let loaded = settings::load_with_diagnostics(&server.brain_home);
    let candidate = match merge(&loaded.settings, &payload) {
        Ok(candidate) => candidate,
        Err(message) => return problem(400, "Bad Request", &message),
    };
    if let Err(message) = candidate.validate() {
        return problem(400, "Bad Request", &message);
    }
    if let Err(error) = settings::save(&server.brain_home, &candidate) {
        return problem(
            500,
            "Internal Server Error",
            &format!("не удалось сохранить настройки: {error}"),
        );
    }
    let mut document = state_document(server, status);
    if let Some(object) = document.as_object_mut() {
        object.insert("saved".to_owned(), Value::Bool(true));
        object.insert(
            "applies_to".to_owned(),
            Value::String("каждую следующую команду brain".to_owned()),
        );
        // Preserve diagnostics from the document that was read immediately
        // before this save. `state_document` now sees the repaired file, so
        // dropping these would turn partial recovery back into a silent one.
        object.insert("settings_warnings".to_owned(), json!(loaded.warnings));
    }
    json_response(200, "OK", &document)
}

/// Folds a submitted document into the stored one.
///
/// A missing key keeps the stored value and an explicit null clears an
/// override, so the page can hand back exactly the knobs it showed without
/// having to restate the ones it did not touch.
fn merge(current: &StoredSettings, payload: &Value) -> Result<StoredSettings, String> {
    let mut next = current.clone();
    if let Some(value) = payload.get("profile") {
        next.profile = value
            .as_str()
            .ok_or_else(|| "профиль должен быть строкой".to_owned())?
            .to_owned();
    }
    if let Some(value) = payload.get("embedding_mode") {
        next.embedding_mode = value
            .as_str()
            .ok_or_else(|| "режим модели должен быть строкой".to_owned())?
            .parse::<EmbeddingMode>()
            .map_err(|_| "режим модели: ожидается always, on-demand или off".to_owned())?;
    }
    if let Some(overrides) = payload.get("overrides") {
        if !overrides.is_object() {
            return Err("раздел overrides должен быть объектом".to_owned());
        }
        next.overrides = QualityOverrides {
            candidates_per_signal: whole(
                overrides,
                "candidates_per_signal",
                current.overrides.candidates_per_signal,
            )?,
            graph_hops: whole(overrides, "graph_hops", current.overrides.graph_hops)?,
            budget_tokens: whole(overrides, "budget_tokens", current.overrides.budget_tokens)?,
            min_cosine: fraction(overrides, "min_cosine", current.overrides.min_cosine)?,
            min_bm25: fraction(overrides, "min_bm25", current.overrides.min_bm25)?,
        };
    }
    Ok(next)
}

/// Reads one optional whole-number override.
fn whole<T: TryFrom<u64>>(
    overrides: &Value,
    key: &str,
    current: Option<T>,
) -> Result<Option<T>, String> {
    match overrides.get(key) {
        None => Ok(current),
        Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|number| T::try_from(number).ok())
            .map(Some)
            .ok_or_else(|| format!("{key}: ожидается целое неотрицательное число")),
    }
}

/// Reads one optional fractional override.
fn fraction(overrides: &Value, key: &str, current: Option<f64>) -> Result<Option<f64>, String> {
    match overrides.get(key) {
        None => Ok(current),
        Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .filter(|number| number.is_finite())
            .map(Some)
            .ok_or_else(|| format!("{key}: ожидается число")),
    }
}

/// Everything the page renders, in one document.
///
/// Deliberately carries no filesystem path, agent name, or memory text: the
/// window shows how the engine is tuned and how much is stored, never what.
fn state_document(server: &Server, status: &StatusRequester) -> Value {
    let loaded = settings::load_with_diagnostics(&server.brain_home);
    let stored = loaded.settings;
    let effective = stored.effective();
    let baseline = brain_core::profile::Profile::Balanced.settings();
    let profiles = brain_core::profile::Profile::all()
        .into_iter()
        .map(|profile| {
            let quality = profile.settings();
            json!({
                "name": profile.to_string(),
                "current": profile == stored.profile(),
                "candidates_per_signal": quality.candidates_per_signal,
                "graph_hops": quality.graph_hops,
                "budget_tokens": quality.budget_tokens,
                "min_cosine": quality.min_cosine,
                "min_bm25": quality.min_bm25,
                "relative_cost": round_hundredths(profile.relative_cost()),
            })
        })
        .collect::<Vec<_>>();

    json!({
        "status": "ok",
        "space": server.space,
        "profile": stored.profile().to_string(),
        "manual": stored.is_manual(),
        "embedding_mode": stored.embedding_mode.to_string(),
        "embedding_runtime_implemented": EMBEDDING_RUNTIME_IMPLEMENTED,
        "settings_warnings": loaded.warnings,
        "profiles": profiles,
        "effective": {
            "candidates_per_signal": effective.candidates_per_signal,
            "graph_hops": effective.graph_hops,
            "budget_tokens": effective.budget_tokens,
            "min_cosine": effective.min_cosine,
            "min_bm25": effective.min_bm25,
        },
        "overrides": stored.overrides,
        "relative_cost": round_hundredths(settings::relative_cost(&effective)),
        "baseline": {
            "candidates_per_signal": baseline.candidates_per_signal,
            "budget_tokens": baseline.budget_tokens,
        },
        "limits": {
            "candidates_per_signal": [CANDIDATES_RANGE.0, CANDIDATES_RANGE.1],
            "graph_hops": [GRAPH_HOPS_RANGE.0, GRAPH_HOPS_RANGE.1],
            "budget_tokens": [BUDGET_TOKENS_RANGE.0, BUDGET_TOKENS_RANGE.1],
            "min_cosine": [MIN_COSINE_RANGE.0, MIN_COSINE_RANGE.1],
            "min_bm25": [MIN_BM25_RANGE.0, MIN_BM25_RANGE.1],
            "graph_hops_diminishing": GRAPH_HOPS_DIMINISHING,
        },
        "database": database_document(server, status),
        "activity": crate::activity::document(&server.brain_home, crate::now_unix_seconds()),
        "work": work_document(server, &server.space),
        // The same read-only observations `brain doctor` prints: configured,
        // observed and imported are different claims and stay separate.
        "capture": crate::doctor::document(&crate::user_home(), &server.brain_home, crate::now_unix_seconds()),
        "api_revision": API_REVISION,
    })
}

/// What the memory is being used for, and what is in the way.
///
/// Both were reachable only from a terminal, which meant the window could show
/// how the engine was tuned but not what anyone was doing with it.
fn work_document(server: &Server, space: &str) -> Value {
    let Ok(connection) = rusqlite::Connection::open_with_flags(
        &server.database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return json!({"available": false});
    };
    let now = crate::now_unix_seconds();
    let goals = brain_core::goals::goals(&connection, space, now).ok();
    let blockers = brain_core::operations::blockers(&connection, space, now, 6).ok();
    json!({
        "available": goals.is_some() || blockers.is_some(),
        "goals": goals,
        "blockers": blockers,
    })
}

/// Live counts, or an honest explanation of why they are missing.
fn database_document(server: &Server, status: &StatusRequester) -> Value {
    match status.fetch(&server.space) {
        Ok(report) => json!({
            "available": true,
            "nodes": report.nodes,
            "active_nodes": report.active_nodes,
            "edges": report.edges,
            "active_edges": report.active_edges,
            "embeddings": report.embeddings,
            "without_embeddings": report.active_nodes.saturating_sub(report.embeddings),
            "salience_layers": {
                "hot": report.hot_nodes,
                "warm": report.warm_nodes,
                "cold": report.cold_nodes,
            },
            "latest_recorded_at": report.latest_recorded_at,
        }),
        Err(message) => json!({"available": false, "error": message}),
    }
}

/// Owns the database on one thread and answers status requests over a channel.
///
/// `SqliteStorage` is neither `Send` nor `Sync`, so sharing one across the
/// connection threads is not possible, and reopening the database on every poll
/// would repeat the migration check several times a minute.
struct StatusService {
    requests: mpsc::Sender<StatusRequest>,
}

type StatusRequest = (String, SyncSender<Result<StorageStatus, String>>);

/// A per-thread handle to the status service.
struct StatusRequester {
    requests: mpsc::Sender<StatusRequest>,
}

impl StatusService {
    fn spawn(database: PathBuf) -> Self {
        let (requests, incoming) = mpsc::channel::<StatusRequest>();
        // Failing to spawn leaves the receiver dropped, which the requester
        // reports as an unavailable database rather than a panic.
        let _ = thread::Builder::new()
            .name("brain-ui-status".to_owned())
            .spawn(move || status_loop(&database, &incoming));
        Self { requests }
    }

    fn requester(&self) -> StatusRequester {
        StatusRequester {
            requests: self.requests.clone(),
        }
    }
}

impl StatusRequester {
    fn fetch(&self, space: &str) -> Result<StorageStatus, String> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.requests
            .send((space.to_owned(), reply))
            .map_err(|_| "служба состояния базы остановлена".to_owned())?;
        answer
            .recv()
            .map_err(|_| "служба состояния базы не ответила".to_owned())?
    }
}

fn status_loop(database: &PathBuf, incoming: &Receiver<StatusRequest>) {
    let storage = SqliteStorage::open(database);
    for (space, reply) in incoming {
        let answer = match &storage {
            Ok(storage) => read_status(storage, &space),
            Err(error) => Err(format!("база недоступна: {error}")),
        };
        let _ = reply.send(answer);
    }
}

fn read_status(storage: &SqliteStorage, space: &str) -> Result<StorageStatus, String> {
    let now = crate::now_unix_seconds();
    storage.flush().map_err(|error| error.to_string())?;
    storage
        .status(space, AsOf::new(now, now))
        .map_err(|error| error.to_string())
}

/// A request token drawn from the operating system's entropy source.
///
/// Deriving it from the clock or the process id would make it guessable by any
/// page that knows roughly when the window was opened, which is the one thing
/// the token exists to prevent.
fn fresh_token() -> Result<String, Box<dyn Error>> {
    #[cfg(unix)]
    {
        let mut bytes = [0_u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }
    #[cfg(not(unix))]
    {
        Err("окно настроек пока поддерживается только там, где есть /dev/urandom".into())
    }
}

/// Compares two tokens without leaking their common prefix through timing.
fn tokens_match(candidate: &str, expected: &str) -> bool {
    if candidate.len() != expected.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in candidate.bytes().zip(expected.bytes()) {
        difference |= left ^ right;
    }
    difference == 0
}

fn find_marker(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn json_response(status: u16, reason: &'static str, value: &Value) -> Response {
    Response {
        status,
        reason,
        content_type: "application/json; charset=utf-8",
        body: serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
    }
}

fn problem(status: u16, reason: &'static str, message: &str) -> Response {
    json_response(
        status,
        reason,
        &json!({"status": "error", "message": message}),
    )
}

fn write_response(stream: &mut TcpStream, response: &Response) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n{SECURITY_HEADERS}\r\n",
        response.status,
        response.reason,
        response.content_type,
        response.body.len(),
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

/// Asks the desktop to open the window, without caring which desktop it is.
fn open_browser(url: &str) -> std::io::Result<()> {
    let (program, leading) = if cfg!(target_os = "macos") {
        ("open", Vec::new())
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", ""])
    } else {
        ("xdg-open", Vec::new())
    };
    std::process::Command::new(program)
        .args(leading)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

fn round_hundredths(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server {
            space: "default".to_owned(),
            brain_home: PathBuf::from("/nonexistent-brain-home"),
            database: PathBuf::from("/nonexistent-brain-home/brain.sqlite"),
            token: "0123456789abcdef".to_owned(),
            generation: "feedfacefeedfacefeedfacefeedface".to_owned(),
            address: SocketAddr::from((Ipv4Addr::LOCALHOST, 41_234)),
        }
    }

    fn request(method: &str, path: &str, headers: &[(&str, &str)]) -> Request {
        Request {
            method: method.to_owned(),
            path: path.split('?').next().unwrap_or_default().to_owned(),
            query: path
                .split_once('?')
                .map(|(_, query)| query.to_owned())
                .unwrap_or_default(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: Vec::new(),
        }
    }

    fn status() -> StatusRequester {
        // The receiver is dropped immediately, so every fetch reports the
        // database as unavailable instead of touching a real file.
        let (requests, _) = mpsc::channel();
        StatusRequester { requests }
    }

    #[test]
    fn a_foreign_host_header_is_refused() {
        let response = route(
            &server(),
            &status(),
            &request("GET", "/", &[("host", "attacker.example")]),
        );
        assert_eq!(
            response.status, 403,
            "DNS rebinding must not reach the page"
        );
    }

    #[test]
    fn the_page_is_served_on_the_loopback_host() {
        let response = route(
            &server(),
            &status(),
            &request("GET", "/", &[("host", "127.0.0.1:41234")]),
        );
        assert_eq!(response.status, 200);
        let body = String::from_utf8(response.body).expect("page is valid UTF-8");
        assert!(
            !body.contains("0123456789abcdef"),
            "the window key must never be in anonymous HTML"
        );
        assert!(
            !body.contains(TOKEN_PLACEHOLDER),
            "no placeholder may survive into the served page"
        );
    }

    #[test]
    fn the_page_asks_for_nothing_outside_itself() {
        for marker in ["http://", "https://", "//fonts.", "//cdn.", "@import"] {
            assert!(
                !PAGE.contains(marker),
                "page must stay self-contained, found {marker:?}"
            );
        }
        // Every reference the browser would follow has to resolve to this
        // server; a single absolute one would turn "no external requests" from
        // a property into a hope.
        for attribute in ["href=\"", "src=\""] {
            for reference in PAGE
                .split(attribute)
                .skip(1)
                .filter_map(|tail| tail.split('"').next())
            {
                assert!(
                    reference.starts_with('/') || reference.starts_with('#'),
                    "page references {reference:?} outside itself"
                );
            }
        }
    }

    #[test]
    fn the_api_needs_the_window_token() {
        let response = route(
            &server(),
            &status(),
            &request("GET", "/api/state", &[("host", "localhost:41234")]),
        );
        assert_eq!(response.status, 403);

        let response = route(
            &server(),
            &status(),
            &request(
                "GET",
                "/api/state",
                &[("host", "localhost:41234"), ("x-brain-token", "wrong")],
            ),
        );
        assert_eq!(response.status, 403);
    }

    #[test]
    fn a_foreign_origin_cannot_submit_a_change() {
        let response = route(
            &server(),
            &status(),
            &request(
                "POST",
                "/api/settings",
                &[
                    ("host", "127.0.0.1:41234"),
                    ("x-brain-token", "0123456789abcdef"),
                    ("origin", "https://evil.example"),
                ],
            ),
        );
        assert_eq!(response.status, 403);
    }

    #[test]
    fn unknown_paths_are_not_served() {
        let response = route(
            &server(),
            &status(),
            &request(
                "GET",
                "/../../etc/passwd",
                &[
                    ("host", "127.0.0.1:41234"),
                    ("x-brain-token", "0123456789abcdef"),
                ],
            ),
        );
        assert_eq!(response.status, 404);
    }

    #[test]
    fn merge_clears_an_override_on_an_explicit_null() {
        let current = StoredSettings {
            overrides: QualityOverrides {
                graph_hops: Some(3),
                budget_tokens: Some(900),
                ..QualityOverrides::default()
            },
            ..StoredSettings::default()
        };
        let merged = merge(&current, &json!({"overrides": {"graph_hops": Value::Null}}))
            .expect("merge accepts the document");
        assert_eq!(merged.overrides.graph_hops, None);
        assert_eq!(
            merged.overrides.budget_tokens,
            Some(900),
            "an untouched knob must keep its value"
        );
    }

    #[test]
    fn merge_rejects_a_negative_candidate_pool() {
        let error = merge(
            &StoredSettings::default(),
            &json!({"overrides": {"candidates_per_signal": -5}}),
        )
        .expect_err("must reject");
        assert!(error.contains("candidates_per_signal"));
    }

    #[test]
    fn the_graph_entries_open_the_explorer_and_the_legacy_viewer_loads_its_assets() {
        let host = [("host", "127.0.0.1:41234")];
        for path in ["/graph", "/graph/3d"] {
            let response = route(&server(), &status(), &request("GET", path, &host));
            assert_eq!(response.status, 200);
            let page = String::from_utf8(response.body).unwrap();
            assert!(
                page.contains("/api/explore/graph") && page.contains("/graph/graph3d.js"),
                "{path}"
            );
            assert!(!page.contains(TOKEN_PLACEHOLDER));
        }
        let script = route(
            &server(),
            &status(),
            &request("GET", "/graph/graph3d.js", &host),
        );
        assert_eq!(
            (script.status, script.content_type),
            (200, "text/javascript; charset=utf-8")
        );
        for text in [EXPLORER_PAGE, GRAPH3D_SCRIPT] {
            for marker in [
                "http://",
                "https://",
                "//cdn.",
                "innerHTML",
                "insertAdjacentHTML",
            ] {
                assert!(!text.contains(marker), "graph page uses {marker}");
            }
        }
        // The old page asks for its assets relative to /graph/; every one of
        // them must answer there, or it never leaves "loading".
        let legacy = route(
            &server(),
            &status(),
            &request("GET", "/graph/legacy-3d", &host),
        );
        assert_eq!(legacy.status, 200);
        // The legacy viewer is a compatibility stub that points at the map.
        assert!(
            String::from_utf8(legacy.body)
                .unwrap()
                .contains("href=\"/map\"")
        );
        for asset in [
            "/graph/vendor/three.min.js",
            "/graph/brain-viewer/app.js",
            "/graph/brain-viewer/style.css",
        ] {
            assert_eq!(
                route(&server(), &status(), &request("GET", asset, &host)).status,
                200,
                "{asset}"
            );
        }
    }

    #[test]
    fn the_drawn_graph_api_needs_the_token_and_links_need_the_origin_too() {
        let (_scratch, database) = crate::explore::tests::chain_database(8);
        let mut live = server();
        live.database = database;
        let host = ("host", "127.0.0.1:41234");
        let token = ("x-brain-token", "0123456789abcdef");
        for path in ["/api/explore/clusters", "/api/explore/graph?limit=3"] {
            assert_eq!(
                route(&live, &status(), &request("GET", path, &[host])).status,
                403,
                "{path}"
            );
            assert_eq!(
                route(&live, &status(), &request("GET", path, &[host, token])).status,
                200,
                "{path}"
            );
        }
        let post = |headers: &[(&str, &str)]| {
            let mut links = request("POST", "/api/explore/links", headers);
            links.body = br#"{"ids":["n1","n2"]}"#.to_vec();
            route(&live, &status(), &links)
        };
        assert_eq!(post(&[host]).status, 403);
        assert_eq!(
            post(&[host, token, ("origin", "http://evil.example")]).status,
            403
        );
        let ok = post(&[host, token, ("origin", "http://127.0.0.1:41234")]);
        assert_eq!(ok.status, 200);
        let body: Value = serde_json::from_slice(&ok.body).unwrap();
        assert_eq!(body["edges_among"], 1);
        let elsewhere = route(
            &live,
            &status(),
            &request("GET", "/api/explore/graph?space=elsewhere", &[host, token]),
        );
        assert_eq!(elsewhere.status, 400);
    }

    #[test]
    fn the_explorer_api_needs_the_window_token_and_maps_errors() {
        for path in [
            "/api/explore/overview",
            "/api/explore/nodes",
            "/api/explore/node?id=x",
        ] {
            let response = route(
                &server(),
                &status(),
                &request("GET", path, &[("host", "127.0.0.1:41234")]),
            );
            assert_eq!(response.status, 403, "{path} answered without a token");
        }
        let (_scratch, database) = crate::explore::tests::chain_database(12);
        let mut live = server();
        live.database = database;
        live.space = "default".to_owned();
        let token = [
            ("host", "127.0.0.1:41234"),
            ("x-brain-token", "0123456789abcdef"),
        ];
        let ok = route(
            &live,
            &status(),
            &request("GET", "/api/explore/nodes?offset=10&limit=5", &token),
        );
        assert_eq!(ok.status, 200);
        let body: Value = serde_json::from_slice(&ok.body).unwrap();
        assert_eq!(body["matched"], 12);
        assert_eq!(body["returned"], 2);
        let bad = route(
            &live,
            &status(),
            &request("GET", "/api/explore/nodes?kind=nope", &token),
        );
        assert_eq!(bad.status, 400);
        let unknown_space = route(
            &live,
            &status(),
            &request("GET", "/api/explore/nodes?space=elsewhere", &token),
        );
        assert_eq!(unknown_space.status, 400);
        let named = route(
            &live,
            &status(),
            &request("GET", "/api/explore/overview?space=default", &token),
        );
        assert_eq!(named.status, 200);
        let missing = route(
            &live,
            &status(),
            &request("GET", "/api/explore/node?id=absent", &token),
        );
        assert_eq!(missing.status, 404);
        let unavailable = route(
            &server(),
            &status(),
            &request("GET", "/api/explore/overview", &token),
        );
        assert_eq!(unavailable.status, 503);
    }

    #[test]
    fn the_ping_states_its_revision_and_old_windows_are_stale() {
        let response = route(
            &server(),
            &status(),
            &request("GET", "/api/ping", &[("host", "127.0.0.1:41234")]),
        );
        let body: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(body["api"], API_REVISION);
        let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n";
        assert_eq!(
            classify(&format!("{head}{body}")),
            Occupant::Current {
                pid: Some(std::process::id()),
                generation: Some("feedfacefeedfacefeedfacefeedface".to_owned()),
            }
        );
        // Revision 6 windows stamped their key into HTML: always replaced.
        assert_eq!(
            classify(&format!(
                "{head}{{\"service\":\"brain-settings\",\"api\":6,\"pid\":7}}"
            )),
            Occupant::Stale(Some(7))
        );
        assert_eq!(
            classify(&format!(
                "{head}{{\"status\":\"ok\",\"service\":\"brain-settings\"}}"
            )),
            Occupant::Stale(None)
        );
        assert_eq!(
            classify(&format!(
                "{head}{{\"service\":\"brain-settings\",\"api\":1,\"pid\":42}}"
            )),
            Occupant::Stale(Some(42))
        );
        assert_eq!(
            classify(&format!("{head}{{\"service\":\"other\",\"api\":9}}")),
            Occupant::Stranger
        );
        assert_eq!(classify("garbage"), Occupant::Stranger);
    }

    #[test]
    fn a_settings_change_round_trips_and_bad_input_leaves_the_file_alone() {
        let (scratch, _) = crate::explore::tests::chain_database(1);
        let mut live = server();
        live.brain_home = scratch.0.clone();
        let headers = [
            ("host", "127.0.0.1:41234"),
            ("x-brain-token", "0123456789abcdef"),
            ("origin", "http://127.0.0.1:41234"),
        ];
        let mut good = request("POST", "/api/settings", &headers);
        good.body =
            br#"{"profile":"fast","embedding_mode":"off","overrides":{"graph_hops":1}}"#.to_vec();
        let saved = route(&live, &status(), &good);
        assert_eq!(saved.status, 200);
        let body: Value = serde_json::from_slice(&saved.body).unwrap();
        assert_eq!(body["saved"], true);
        assert_eq!(body["profile"], "fast");
        let stored = settings::load(&live.brain_home);
        assert_eq!(stored.embedding_mode.to_string(), "off");
        assert_eq!(stored.overrides.graph_hops, Some(1));
        let before = std::fs::read(settings::settings_path(&live.brain_home)).unwrap();
        for payload in [
            &br#"{"embedding_mode":"sometimes"}"#[..],
            br#"{"overrides":{"candidates_per_signal":-5}}"#,
            br#"{"overrides":[]}"#,
            b"not json",
        ] {
            let mut bad = request("POST", "/api/settings", &headers);
            bad.body = payload.to_vec();
            assert_eq!(
                route(&live, &status(), &bad).status,
                400,
                "{}",
                String::from_utf8_lossy(payload)
            );
        }
        assert_eq!(
            std::fs::read(settings::settings_path(&live.brain_home)).unwrap(),
            before
        );
    }

    #[test]
    fn the_archive_api_needs_the_token_and_reports_a_missing_archive_honestly() {
        for path in [
            "/api/archive/status",
            "/api/archive/search?q=x",
            "/api/archive/read?id=1",
        ] {
            let response = route(
                &server(),
                &status(),
                &request("GET", path, &[("host", "127.0.0.1:41234")]),
            );
            assert_eq!(response.status, 403, "{path}");
            let foreign = route(
                &server(),
                &status(),
                &request(
                    "GET",
                    path,
                    &[
                        ("host", "evil.example:41234"),
                        ("x-brain-token", "0123456789abcdef"),
                    ],
                ),
            );
            assert_eq!(foreign.status, 403, "{path}");
        }
        let token = [
            ("host", "127.0.0.1:41234"),
            ("x-brain-token", "0123456789abcdef"),
        ];
        let status_doc = route(
            &server(),
            &status(),
            &request("GET", "/api/archive/status", &token),
        );
        assert_eq!(status_doc.status, 200);
        let body: Value = serde_json::from_slice(&status_doc.body).expect("json");
        assert_eq!(body["status"], "not_configured");
        assert_eq!(body["complete_capture_claimed"], false);
        let search = route(
            &server(),
            &status(),
            &request("GET", "/api/archive/search?q=x", &token),
        );
        assert_eq!(search.status, 503);
        let bad = route(
            &server(),
            &status(),
            &request("GET", "/api/archive/search?q=x&limit=abc", &token),
        );
        assert_eq!(bad.status, 400);
        let bad_id = route(
            &server(),
            &status(),
            &request("GET", "/api/archive/read?id=x%27", &token),
        );
        assert_eq!(bad_id.status, 400);
    }

    #[test]
    fn the_settings_page_renders_archive_text_without_html_injection() {
        let response = route(
            &server(),
            &status(),
            &request("GET", "/", &[("host", "127.0.0.1:41234")]),
        );
        let page = String::from_utf8(response.body).expect("utf8");
        let archive = page
            .split("function renderArchive")
            .nth(1)
            .expect("archive renderer");
        let archive = archive
            .split("\nfunction renderDiagnostics()")
            .next()
            .expect("end");
        assert!(
            !archive.contains("innerHTML"),
            "archive code must use textContent"
        );
        assert!(page.contains("/api/archive/search"));
    }

    #[test]
    fn the_workboard_is_token_gated_read_only_and_paged() {
        let path = "/api/work/board";
        assert_eq!(
            route(
                &server(),
                &status(),
                &request("GET", path, &[("host", "127.0.0.1:41234")])
            )
            .status,
            403
        );
        assert_eq!(
            route(
                &server(),
                &status(),
                &request(
                    "POST",
                    path,
                    &[
                        ("host", "127.0.0.1:41234"),
                        ("x-brain-token", "0123456789abcdef")
                    ]
                )
            )
            .status,
            404
        );
        let token = [
            ("host", "127.0.0.1:41234"),
            ("x-brain-token", "0123456789abcdef"),
        ];
        let missing = route(&server(), &status(), &request("GET", path, &token));
        assert_eq!(missing.status, 200);
        assert_eq!(
            serde_json::from_slice::<Value>(&missing.body).unwrap()["status"],
            "not_configured"
        );

        let home = std::env::temp_dir().join(format!("brain-ui-work-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let db = brain_core::coordination::database_path(&home);
        let actor = brain_core::coordination::Actor::new("default", "codex", "m-aaa").unwrap();
        let mut coordinator = brain_core::coordination::Coordinator::open(&db).unwrap();
        for n in 0..3 {
            coordinator
                .add_task(&actor, "p", &format!("<b>t{n}</b>"), "", &[], None)
                .unwrap();
        }
        coordinator.claim_task(&actor, 1, 60, None).unwrap();
        let live = Server {
            brain_home: home.clone(),
            ..server()
        };
        let page = route(
            &live,
            &status(),
            &request("GET", "/api/work/board?limit=2", &token),
        );
        let body: Value = serde_json::from_slice(&page.body).unwrap();
        assert_eq!(body["tasks"].as_array().unwrap().len(), 2);
        assert_eq!(body["has_more"], true);
        assert_eq!(body["tasks"][0]["owner"]["machine"], "m-aaa");
        assert_eq!(body["claims"].as_array().unwrap().len(), 1);
        assert_eq!(
            route(
                &live,
                &status(),
                &request("GET", "/api/work/board?limit=x", &token)
            )
            .status,
            400
        );
        assert_eq!(
            route(
                &live,
                &status(),
                &request("GET", "/api/work/board?state=bogus", &token)
            )
            .status,
            400
        );
        let _ = std::fs::remove_dir_all(&home);

        let html = String::from_utf8(
            route(
                &server(),
                &status(),
                &request("GET", "/", &[("host", "127.0.0.1:41234")]),
            )
            .body,
        )
        .unwrap();
        let work = html
            .split("const WORK_STATES")
            .nth(1)
            .expect("work renderer");
        assert!(
            !work.contains("innerHTML"),
            "workboard must use textContent"
        );
    }

    #[test]
    fn tokens_of_different_length_never_match() {
        assert!(!tokens_match("abc", "abcd"));
        assert!(tokens_match("abcd", "abcd"));
    }

    #[test]
    fn the_archive_backfill_route_is_token_gated_and_readonly_on_get() {
        let host = [("host", "127.0.0.1:41234")];
        assert_eq!(
            route(
                &server(),
                &status(),
                &request("GET", "/api/archive/backfill", &host)
            )
            .status,
            403
        );
        let token = [
            ("host", "127.0.0.1:41234"),
            ("x-brain-token", "0123456789abcdef"),
        ];
        let response = route(
            &server(),
            &status(),
            &request("GET", "/api/archive/backfill", &token),
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_slice(&response.body).expect("json");
        assert_eq!(body["complete_capture_claimed"], false);
        assert_eq!(body["backfill"]["state"], "disabled");
        // The read never creates anything in a home that does not exist.
        assert!(!PathBuf::from("/nonexistent-brain-home/archive/backfill.json").exists());
    }

    #[test]
    fn archive_backfill_post_requires_token_and_origin_and_writes_only_via_start() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let home = std::env::temp_dir().join(format!("brain-ui-backfill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("archive")).expect("dir");
        std::fs::set_permissions(home.join("archive"), std::fs::Permissions::from_mode(0o700))
            .expect("chmod dir");
        let mut live = server();
        live.brain_home = home.clone();
        let backfill = home.join("archive/backfill.json");

        // headers: (host, token?, origin?); body is the raw JSON.
        let post = |headers: &[(&str, &str)], raw: &str| {
            let mut request = request("POST", "/api/archive/backfill", headers);
            request.body = raw.as_bytes().to_vec();
            route(&live, &status(), &request)
        };
        let start = r#"{"action":"start","files_per_run":2}"#;
        // No headers at all, host only, and a foreign origin: all refused, no write.
        for headers in [
            &[][..],
            &[("host", "127.0.0.1:41234")][..],
            &[
                ("host", "127.0.0.1:41234"),
                ("x-brain-token", "0123456789abcdef"),
                ("origin", "http://evil.example"),
            ][..],
        ] {
            assert_eq!(post(headers, start).status, 403, "{headers:?}");
        }
        assert!(
            !backfill.exists(),
            "an unauthenticated request must not write config"
        );

        let good = [
            ("host", "127.0.0.1:41234"),
            ("x-brain-token", "0123456789abcdef"),
            ("origin", "http://127.0.0.1:41234"),
        ];
        // Authenticated but malformed: every one is a 400 and nothing is written.
        for raw in [
            r#"{"action":"start","files_per_run":2,"extra":1}"#,
            r#"{"action":"start","files_per_run":0}"#,
            r#"{"action":"start","files_per_run":21}"#,
            r#"{"action":"start","files_per_run":"2"}"#,
            r#"{"action":"start","files_per_run":2.5}"#,
            r#"{"action":"pause"}"#,
            r#"{"action":"start"}"#,
            "not json",
            "[1,2]",
        ] {
            assert_eq!(post(&good, raw).status, 400, "{raw}");
        }
        assert!(!backfill.exists(), "invalid bodies must never write config");

        // A valid start writes the file (owner-only) and reports the state.
        let saved = post(&good, r#"{"action":"start","files_per_run":3}"#);
        assert_eq!(saved.status, 200);
        let body: Value = serde_json::from_slice(&saved.body).expect("json");
        assert_eq!(
            (body["state"].clone(), body["files_per_run"].clone()),
            (json!("running"), json!(3))
        );
        assert_eq!(
            std::fs::metadata(&backfill).expect("meta").mode() & 0o777,
            0o600
        );

        // Stop flips the state but keeps the file; GET reflects it.
        assert_eq!(post(&good, r#"{"action":"stop"}"#).status, 200);
        let read = route(
            &live,
            &status(),
            &request("GET", "/api/archive/backfill", &good),
        );
        let read: Value = serde_json::from_slice(&read.body).expect("json");
        assert_eq!(read["backfill"]["state"], "stopped");
        let _ = std::fs::remove_dir_all(&home);
    }

    fn map_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("brain-ui-map-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn map_server(home: &std::path::Path) -> Server {
        Server {
            brain_home: home.to_path_buf(),
            database: home.join("brain.sqlite"),
            ..server()
        }
    }

    fn body(response: &Response) -> Value {
        serde_json::from_slice(&response.body).unwrap()
    }

    #[test]
    fn the_map_page_is_reachable_self_contained_and_never_injects_html() {
        let host = [("host", "127.0.0.1:41234")];
        let page = route(&server(), &status(), &request("GET", "/map", &host));
        assert_eq!(page.status, 200);
        let html = String::from_utf8(page.body).unwrap();
        assert!(!html.contains(TOKEN_PLACEHOLDER), "the key is stamped in");
        assert!(
            html.contains("/api/map/projects")
                && html.contains("/api/map/timeline")
                && html.contains("/api/map/packet")
        );
        assert!(
            !html.contains("innerHTML")
                && !html.contains("insertAdjacentHTML")
                && !html.contains("eval(")
        );
        assert!(
            !html.contains("http://") && !html.contains("https://"),
            "no external requests"
        );
        assert!(
            !html.to_lowercase().contains("подтвердить"),
            "no confirmation control exists yet"
        );
        for (path, page) in [("/graph", EXPLORER_PAGE), ("/", PAGE)] {
            assert!(page.contains("href=\"/map\""), "{path} links to the map");
        }
    }

    #[test]
    fn the_map_serves_its_3d_script_and_the_page_offers_a_2d_3d_switch() {
        let host = [("host", "127.0.0.1:41234")];
        // A static asset with no state: served without the window key, like the
        // graph's script, and never carrying the token placeholder.
        let script = route(
            &server(),
            &status(),
            &request("GET", "/map/map3d.js", &host),
        );
        assert_eq!(
            (script.status, script.content_type),
            (200, "text/javascript; charset=utf-8")
        );
        let js = String::from_utf8(script.body).unwrap();
        assert!(!js.contains(TOKEN_PLACEHOLDER));
        for marker in [
            "http://",
            "https://",
            "innerHTML",
            "insertAdjacentHTML",
            "eval(",
        ] {
            assert!(!js.contains(marker), "the 3D script uses {marker}");
        }
        // It exposes the one projection and the caps the page relies on.
        assert!(js.contains("window.BrainMap3D") && js.contains("function project"));
        assert!(
            js.contains("/map/map3d.js") || js.contains("project:"),
            "layout markers survive"
        );
        // The page loads the script and offers a real 2D/3D switch.
        let page =
            String::from_utf8(route(&server(), &status(), &request("GET", "/map", &host)).body)
                .unwrap();
        assert!(
            page.contains("/map/map3d.js"),
            "the map page loads the 3D script"
        );
        assert!(
            page.contains("id=\"mode-3d\"") && page.contains("id=\"mode-2d\""),
            "a 2D/3D switch exists"
        );
        assert!(!page.contains(TOKEN_PLACEHOLDER));
    }

    #[test]
    fn map_reads_need_the_key_and_never_create_the_map() {
        let home = map_home("absent");
        let live = map_server(&home);
        let host = ("host", "127.0.0.1:41234");
        assert_eq!(
            route(
                &live,
                &status(),
                &request("GET", "/api/map/status", &[host])
            )
            .status,
            403
        );
        let token = [host, ("x-brain-token", "0123456789abcdef")];
        for path in [
            "/api/map/status",
            "/api/map/projects",
            "/api/map/project?project=A",
            "/api/map/timeline?task=1",
            "/api/map/links",
        ] {
            let r = route(&live, &status(), &request("GET", path, &token));
            assert_eq!(r.status, 200, "{path}");
            let v = body(&r);
            assert!(
                v["map"] == "not_configured"
                    || v["status"] == "not_configured"
                    || v["links"]["map"] == "not_configured",
                "{path}: {v}"
            );
        }
        let packet = body(&route(
            &live,
            &status(),
            &request("GET", "/api/map/packet?project=A", &token),
        ));
        let raw = packet["packet_json"].as_str().unwrap();
        assert_eq!(packet["packet_bytes"].as_u64().unwrap() as usize, raw.len());
        assert!(raw.contains("\"map\":\"not_configured\""));
        for bad in [
            "/api/map/projects?space=a%20b",
            "/api/map/projects?limit=0",
            "/api/map/projects?limit=101",
            "/api/map/project?project=..%2Fx",
            "/api/map/project",
            "/api/map/timeline?task=x",
            "/api/map/packet?tier=huge",
            "/api/map/links?cursor=nope",
        ] {
            assert_eq!(
                route(&live, &status(), &request("GET", bad, &token)).status,
                400,
                "{bad}"
            );
        }
        assert_eq!(
            route(
                &live,
                &status(),
                &request("GET", "/api/map/nowhere", &token)
            )
            .status,
            404
        );
        assert!(
            !home.join("map").exists() && !home.join("machine-id").exists(),
            "reads created files"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn map_writes_need_key_and_origin_and_projects_show_phases_and_paging() {
        use brain_core::coordination::{Actor, Coordinator, database_path};
        let home = map_home("flow");
        let live = map_server(&home);
        let mut c = Coordinator::open(&database_path(&home)).unwrap();
        let a = Actor::new("default", "codex", "m-aaa").unwrap();
        let mut ids = Vec::new();
        for (project, title) in [
            ("A", "one"),
            ("A", "two"),
            ("A", "<b>three</b>"),
            ("B", "beta"),
        ] {
            ids.push(
                c.add_task(&a, project, title, "", &[], None).unwrap()["task"]["id"]
                    .as_i64()
                    .unwrap(),
            );
        }
        let claim = c.claim_task(&a, ids[0], 60, None).unwrap();
        let (cid, fence) = (
            claim["claim"]["claim_id"].as_i64().unwrap(),
            claim["claim"]["fence"].as_i64().unwrap(),
        );
        c.set_state(&a, cid, fence, "completed", "", None).unwrap();
        c.claim_task(&a, ids[1], 600, None).unwrap();
        drop(c);
        let host = ("host", "127.0.0.1:41234");
        let token = ("x-brain-token", "0123456789abcdef");
        let origin = ("origin", "http://127.0.0.1:41234");
        for headers in [
            &[host, token][..],
            &[host, origin],
            &[host, token, ("origin", "http://evil.example")],
        ] {
            let r = route(&live, &status(), &request("POST", "/api/map/init", headers));
            assert_eq!(r.status, 403, "{headers:?}");
        }
        assert!(!home.join("map").exists());
        let projects = body(&route(
            &live,
            &status(),
            &request("GET", "/api/map/projects", &[host, token]),
        ));
        let a_row = projects["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "A")
            .unwrap()
            .clone();
        assert_eq!(
            (
                a_row["past"].as_u64(),
                a_row["current"].as_u64(),
                a_row["planned"].as_u64()
            ),
            (Some(1), Some(1), Some(1))
        );
        assert_eq!(projects["map"], "not_configured");
        // Ingest before init is refused: nothing creates the map but init.
        let refused = route(
            &live,
            &status(),
            &request("POST", "/api/map/ingest", &[host, token, origin]),
        );
        assert_eq!(refused.status, 409);
        let created = route(
            &live,
            &status(),
            &request("POST", "/api/map/init", &[host, token, origin]),
        );
        assert_eq!(
            (created.status, body(&created)["map"].as_str()),
            (200, Some("ready"))
        );
        let ingested = body(&route(
            &live,
            &status(),
            &request("POST", "/api/map/ingest", &[host, token, origin]),
        ));
        assert!(ingested["appended"].as_u64().unwrap() >= 6, "{ingested}");
        let first = body(&route(
            &live,
            &status(),
            &request("GET", "/api/map/project?project=A&limit=2", &[host, token]),
        ));
        assert_eq!(
            (
                first["tasks"].as_array().unwrap().len(),
                first["has_more"].as_bool()
            ),
            (2, Some(true))
        );
        let rest = body(&route(
            &live,
            &status(),
            &request(
                "GET",
                "/api/map/project?project=A&limit=2&offset=2",
                &[host, token],
            ),
        ));
        assert_eq!(rest["tasks"].as_array().unwrap().len(), 1);
        let phases: Vec<&str> = first["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .chain(rest["tasks"].as_array().unwrap())
            .map(|t| t["phase"].as_str().unwrap())
            .collect();
        let mut sorted = phases.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec!["current", "past", "planned"]);
        assert!(
            rest["tasks"][0]["title"] == "<b>three</b>"
                || first["tasks"].to_string().contains("<b>three</b>"),
            "titles are data, shown as text"
        );
        let timeline = body(&route(
            &live,
            &status(),
            &request(
                "GET",
                &format!("/api/map/timeline?task={}", ids[0]),
                &[host, token],
            ),
        ));
        let outcomes: Vec<&str> = timeline["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["outcome"].as_str())
            .filter(|o| !o.is_empty())
            .collect();
        assert_eq!(outcomes, vec!["claimed_completed"]);
        let packet = body(&route(
            &live,
            &status(),
            &request("GET", "/api/map/packet?project=B&tier=weak", &[host, token]),
        ));
        let raw = packet["packet_json"].as_str().unwrap();
        assert!(
            raw.len() <= 2_048
                && raw.contains("project:default/B")
                && !raw.contains("task:default/1\"")
        );
        let elsewhere = body(&route(
            &live,
            &status(),
            &request("GET", "/api/map/projects?space=elsewhere", &[host, token]),
        ));
        assert!(
            elsewhere["projects"].as_array().unwrap().is_empty(),
            "another space shows nothing of this one"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn anonymous_html_never_carries_the_key_and_anonymous_data_is_refused() {
        let host = ("host", "127.0.0.1:41234");
        for path in ["/", "/graph", "/graph/3d", "/graph/legacy-3d", "/map"] {
            let response = route(&server(), &status(), &request("GET", path, &[host]));
            assert_eq!(response.status, 200, "{path}");
            let page = String::from_utf8(response.body).unwrap();
            assert!(!page.contains("0123456789abcdef"), "{path} leaks the key");
            assert!(!page.contains(TOKEN_PLACEHOLDER), "{path}");
        }
        for page in [PAGE, EXPLORER_PAGE, MAP_PAGE] {
            assert!(page.contains("sessionStorage") && page.contains("history.replaceState"));
        }
        for path in [
            "/brain-data.json",
            "/graph/brain-data.json",
            "/api/state",
            "/api/graph",
            "/api/explore/overview",
            "/api/map/projects",
            "/api/work/board",
            "/api/archive/status",
        ] {
            let response = route(&server(), &status(), &request("GET", path, &[host]));
            assert_eq!(response.status, 403, "{path} answered anonymously");
            let wrong = route(
                &server(),
                &status(),
                &request("GET", path, &[host, ("x-brain-token", "0123456789abcdee")]),
            );
            assert_eq!(wrong.status, 403, "{path} took a wrong key");
        }
        let owned = route(
            &server(),
            &status(),
            &request(
                "GET",
                "/brain-data.json",
                &[host, ("x-brain-token", "0123456789abcdef")],
            ),
        );
        assert_eq!(owned.status, 200);
        // The ping says nothing secret.
        let ping = route(&server(), &status(), &request("GET", "/api/ping", &[host]));
        assert!(
            !String::from_utf8(ping.body)
                .unwrap()
                .contains("0123456789abcdef")
        );
        assert!(owner_url(8765, "/map", "ab").ends_with("/map#k=ab"));
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_or_unprivate_owner_file_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!(
            "brain-ui-owner-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let live = Server {
            brain_home: home.clone(),
            token: "a".repeat(64),
            ..server()
        };
        let path = crate::private_fs::tests::with_open_umask(|| write_owner_file(&live)).unwrap();
        assert_eq!(crate::private_fs::tests::mode(&path), 0o600);
        assert_eq!(
            crate::private_fs::tests::mode(path.parent().unwrap()),
            0o700
        );
        assert_eq!(crate::private_fs::tests::mode(&home), 0o700);
        let generation = "feedfacefeedfacefeedfacefeedface";
        let me = Some(std::process::id());
        // Another pid or generation than the window reports: refused.
        assert!(recover_owner_token(&home, 41_234, Some(1), Some(generation)).is_err());
        assert!(recover_owner_token(&home, 41_234, me, Some("other")).is_err());
        assert!(recover_owner_token(&home, 41_234, None, None).is_err());
        assert!(recover_owner_token(&home, 41_235, me, Some(generation)).is_err());
        // Matching file, but this test process is not a listening brain ui.
        #[cfg(target_os = "linux")]
        assert!(recover_owner_token(&home, 41_234, me, Some(generation)).is_err());
        // A group-readable owner file is never trusted.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(recover_owner_token(&home, 41_234, me, Some(generation)).is_err());
        // Nor is a symlink in its place.
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", &path).unwrap();
        assert!(recover_owner_token(&home, 41_234, me, Some(generation)).is_err());
        std::fs::remove_dir_all(home).unwrap();
    }
}
