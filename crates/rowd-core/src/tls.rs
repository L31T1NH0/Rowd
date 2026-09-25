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
    invite.validate()?;
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(hex::decode(&invite.cert_der)?))?;
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let addresses: Vec<_> = invite.address.to_socket_addrs()?.collect();
    let socket = addresses
        .iter()
        .find_map(|addr| TcpStream::connect_timeout(addr, Duration::from_secs(5)).ok())
        .context("PC unavailable; check Wi-Fi, address and firewall")?;
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
