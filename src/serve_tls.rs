use crate::kafka::Kafka;
use crate::settings::*;
use crate::{read_logs, ConnectionState};
/**
 * This module handles the necessary configuration to serve over TLS
 */
use async_std::{
    io,
    io::BufReader,
    net::{TcpListener, TcpStream, ToSocketAddrs},
    prelude::*,
    sync::Arc,
    task,
};
use async_tls::TlsAcceptor;
use crossbeam::channel::bounded;
use dipstick::*;
use log::*;
use rustls::internal::pemfile::{certs, rsa_private_keys};
use rustls::{Certificate, NoClientAuth, PrivateKey, ServerConfig};
use std::path::Path;

/// Load the passed certificates file
fn load_certs(path: &Path) -> io::Result<Vec<Certificate>> {
    debug!("Loading TLS certs from: {}", path.display());
    certs(&mut std::io::BufReader::new(std::fs::File::open(path)?))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid cert"))
}

/// Load the passed keys file
fn load_keys(path: &Path) -> io::Result<Vec<PrivateKey>> {
    debug!("Loading TLS keys from: {}", path.display());
    rsa_private_keys(&mut std::io::BufReader::new(std::fs::File::open(path)?))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid key"))
}

/// Configure the server using rusttls
/// See https://docs.rs/rustls/0.16.0/rustls/struct.ServerConfig.html for details
///
/// A TLS server needs a certificate and a fitting private key
fn load_tls_config(settings: &Settings) -> io::Result<ServerConfig> {
    match &settings.global.listen.tls {
        TlsType::CertAndKey { cert, key } => {
            let certs = load_certs(cert.as_path())?;
            let mut keys = load_keys(key.as_path())?;

            // we don't use client authentication
            let mut config = ServerConfig::new(NoClientAuth::new());
            config
                // set this server to use one cert together with the loaded private key
                .set_single_cert(certs, keys.remove(0))
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;

            Ok(config)
        }
        _ => {
            panic!("Attempted to load a TLS configuration despite TLS not being enabled");
        }
    }
}

pub async fn accept_loop(
    addr: impl ToSocketAddrs,
    settings: Arc<Settings>,
    metrics: Arc<LockingOutput>,
) -> Result<()> {
    let config = load_tls_config(&settings)?;

    let mut kafka = Kafka::new(settings.global.kafka.buffer);

    if !kafka.connect(
        &settings.global.kafka.conf,
        Some(settings.global.kafka.timeout_ms),
    ) {
        error!("Cannot start hotdog without a workable broker connection");
        return Ok(());
    }

    kafka.with_metrics(metrics.clone());

    let sender = kafka.get_sender();

    task::spawn(async move {
        debug!("starting sendloop");
        kafka.sendloop();
    });
    // We create one TLSAcceptor around a shared configuration.
    // Cloning the acceptor will not clone the configuration.
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind(&addr).await?;
    let mut incoming = listener.incoming();

    /*
     * This crossbeam channel is only useful for keeping track of the connection counts
     */
    let (conn_tx, conn_rx) = bounded::<i64>(1);
    let counter = metrics.gauge("connections");

    /*
     * TODO: A full thread for this seems like a waste
     */
    std::thread::spawn(move || {
        let mut connections = 0;

        loop {
            if let Ok(count) = conn_rx.recv() {
                connections = connections + count;
                debug!("Connection count now {}", connections);
                counter.value(connections);
            }
        }
    });

    while let Some(stream) = incoming.next().await {
        // Add a connection to the gauge
        conn_tx.send(1).unwrap();

        // We use one acceptor per connection, so
        // we need to clone the current one.
        let acceptor = acceptor.clone();
        let mut stream = stream?;

        let state = ConnectionState {
            settings: settings.clone(),
            metrics: metrics.clone(),
            sender: sender.clone(),
        };

        let ctx = conn_tx.clone();

        task::spawn(async move {
            handle_connection(&acceptor, &mut stream, state).await;
            ctx.send(-1).unwrap();
        });
    }
    Ok(())
}

/// The connection handling function.
async fn handle_connection(
    acceptor: &TlsAcceptor,
    tcp_stream: &mut TcpStream,
    state: ConnectionState,
) -> io::Result<()> {
    let peer_addr = tcp_stream.peer_addr()?;
    debug!("Accepted connection from: {}", peer_addr);

    // Calling `acceptor.accept` will start the TLS handshake
    let handshake = acceptor.accept(tcp_stream);
    // The handshake is a future we can await to get an encrypted
    // stream back.
    let tls_stream = handshake.await?;
    let reader = BufReader::new(tls_stream);

    read_logs(reader, state).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_certs() {
        let cert_path = Path::new("./contrib/cert.pem");
        if let Ok(certs) = load_certs(&cert_path) {
            assert_eq!(1, certs.len());
        } else {
            assert!(false);
        }
    }

    #[test]
    fn test_load_keys() {
        let key_path = Path::new("./contrib/cert-key.pem");
        if let Ok(keys) = load_keys(&key_path) {
            assert_eq!(1, keys.len());
        } else {
            assert!(false);
        }
    }
}