//! Network transport for the Brain coordinator: `brain-coord/1`.
//!
//! Stage scope: typed protocol, auth store, a loopback plaintext server and
//! a loopback TLS server/client (pinned CA, numeric dial target, absolute
//! deadline) driven by `brain net`. Binding a LAN interface is a separate
//! deployment gate that this module does not offer.

pub mod auth;
pub mod client;
pub mod protocol;
pub mod server;
pub mod tls;

pub use auth::{AuthStore, Authenticated, BearerToken, Principal, TokenId};
pub use client::{
    CallResult, ClientConfig, CoordClient, read_credential, read_credential_from,
    write_credential_file,
};
pub use protocol::{CoordRequest, NetError, allowed_tools, is_write_tool, tools_for_role};
pub use server::{BindMode, CallOutcome, CoordServer, SHARE_STORE_FILE, ServerConfig};
pub use tls::{
    Dialer, KeySource, TcpDialer, TlsCallResult, TlsClientConfig, TlsCoordClient, TlsCoordServer,
    TlsError, TlsServerConfig, TlsServerHandle, open_inherited_fd, parse_dial_target,
};
