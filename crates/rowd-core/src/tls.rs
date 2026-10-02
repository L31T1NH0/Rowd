use crate::model::Invitation;
use anyhow::{Context, Result};
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};
use std::{
    net::{TcpStream, ToSocketAddrs},
    sync::Arc,
    time::Duration,
};

pub type ClientStream = StreamOwned<ClientConnection, TcpStream>;

pub fn server_config(cert: &str, key: &str) -> Result<Arc<ServerConfig>> {
    Ok(Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(hex::decode(cert)?)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(hex::decode(key)?)),
            )?,
    ))
}
pub fn accept(
    socket: TcpStream,
    config: Arc<ServerConfig>,
) -> Result<StreamOwned<ServerConnection, TcpStream>> {
    set_timeout(&socket)?;
    Ok(StreamOwned::new(ServerConnection::new(config)?, socket))
}
pub fn connect(invite: &Invitation) -> Result<StreamOwned<ClientConnection, TcpStream>> {
    connect_to(invite, &invite.address, Duration::from_secs(5))
}
pub fn connect_to(invite: &Invitation, endpoint: &str, timeout: Duration) -> Result<ClientStream> {
    invite.validate()?;
    connect_pinned(&invite.cert_der, endpoint, timeout)
}

pub fn connect_pinned(cert_der: &str, endpoint: &str, timeout: Duration) -> Result<ClientStream> {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(hex::decode(cert_der)?))?;
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    crate::trace_event!(
        crate::trace::Level::Debug,
        crate::trace::Component::Connection,
        "SOCKET_CONNECT_START",
        serde_json::json!({"endpoint":endpoint,"timeout_ms":timeout.as_millis()})
    );
    let addresses: Vec<_> = endpoint.to_socket_addrs()?.collect();
    let socket = addresses
        .iter()
        .find_map(|addr| match TcpStream::connect_timeout(addr, timeout) {
            Ok(socket)=>Some(socket),
            Err(error)=> {let error=anyhow::Error::new(error);crate::trace_event!(crate::trace::Level::Warn,crate::trace::Component::Connection,"SOCKET_CONNECT_END",serde_json::json!({"endpoint":addr.to_string(),"result":"failed","error":crate::trace::TraceError::new("connection","socket_connect",&error)}));None}
        })
        .context("PC unavailable; check Wi-Fi, address and firewall")?;
    crate::trace_event!(
        crate::trace::Level::Debug,
        crate::trace::Component::Connection,
        "SOCKET_CONNECT_END",
        serde_json::json!({"endpoint":endpoint,"result":"connected"})
    );
    crate::trace_event!(
        crate::trace::Level::Debug,
        crate::trace::Component::Connection,
        "TLS_HANDSHAKE_START",
        serde_json::json!({"endpoint":endpoint,"mode":"lazy"})
    );
    set_timeout(&socket)?;
    let name = ServerName::try_from("rowd.local")?;
    Ok(StreamOwned::new(
        ClientConnection::new(Arc::new(config), name)?,
        socket,
    ))
}
fn set_timeout(socket: &TcpStream) -> Result<()> {
    socket.set_read_timeout(Some(Duration::from_secs(90)))?;
    socket.set_write_timeout(Some(Duration::from_secs(90)))?;
    socket.set_nodelay(true)?;
    Ok(())
}
