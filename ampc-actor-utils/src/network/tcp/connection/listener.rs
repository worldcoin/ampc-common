use std::collections::HashMap;
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tokio_rustls::rustls::pki_types::{pem::PemObject, CertificateDer};
use tokio_util::sync::CancellationToken;

use crate::{
    execution::player::Identity,
    network::tcp::{connection::handshake, ConnectionId, NetworkConnection, Server, TlsConfig},
};

pub struct ConnectionRequest<T: NetworkConnection> {
    peer_id: Identity,
    connection_id: ConnectionId,
    rsp: oneshot::Sender<T>,
}

impl<T: NetworkConnection> ConnectionRequest<T> {
    pub fn new(peer_id: Identity, connection_id: ConnectionId, rsp: oneshot::Sender<T>) -> Self {
        Self {
            peer_id,
            connection_id,
            rsp,
        }
    }
}

pub async fn accept_loop<T: NetworkConnection, S: Server<Output = T>>(
    listener: S,
    mut cmd_ch: UnboundedReceiver<ConnectionRequest<T>>,
    shutdown_ct: CancellationToken,
    tls: Option<TlsConfig>,
) {
    let mut connection_requests: HashMap<Identity, HashMap<ConnectionId, oneshot::Sender<T>>> =
        HashMap::new();

    // Pre-load expected peer leaf certs (as DER bytes) for cert-pinning validation.
    let leaf_cert_cache: Option<HashMap<String, Vec<u8>>> = match &tls {
        Some(tls_cfg) if !tls_cfg.leaf_certs.is_empty() => {
            let mut cache = HashMap::new();
            #[allow(clippy::iter_over_hash_type)]
            for (identity, cert_path) in &tls_cfg.leaf_certs {
                match CertificateDer::pem_file_iter(cert_path) {
                    Err(e) => {
                        tracing::error!(identity, cert_path, "failed to open leaf cert file: {e}")
                    }
                    Ok(mut iter) => match iter.next() {
                        None => tracing::error!(
                            identity,
                            cert_path,
                            "leaf cert file contains no certificates"
                        ),
                        Some(Err(e)) => {
                            tracing::error!(identity, cert_path, "failed to parse leaf cert: {e}")
                        }
                        Some(Ok(cert)) => {
                            cache.insert(identity.clone(), cert.as_ref().to_vec());
                        }
                    },
                }
            }
            Some(cache)
        }
        _ => None,
    };

    loop {
        let r = tokio::select! {
            res = listener.accept() => res,
            cmd = cmd_ch.recv() => {
                if let Some(cmd) = cmd {
                    let peer_map = connection_requests.entry(cmd.peer_id).or_default();
                    peer_map.insert(cmd.connection_id, cmd.rsp);
                    continue;
                } else {
                    tracing::warn!("shutting down accept loop: cmd_ch closed");
                    break;
                }
            },
            _ = shutdown_ct.cancelled() => {
                break;
            }
        };
        match r {
            Ok((_peer_addr, mut stream)) => {
                let (peer_id, connection_id) = match handshake::inbound(&mut stream).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::debug!("application level handshake failed: {e:?}");
                        stream.close().await;
                        continue;
                    }
                };

                // Cert-pinning: if leaf_certs are configured, verify the peer presented the
                // expected certificate for its claimed identity.
                if let Some(ref cache) = leaf_cert_cache {
                    let identity = &peer_id.0;
                    let valid = cache
                        .get(identity)
                        .map(|expected| stream.maybe_tls_cert() == Some(expected.as_slice()))
                        .unwrap_or(false);
                    if !valid {
                        tracing::warn!(
                            peer_id = identity,
                            "peer cert validation failed: rejecting connection"
                        );
                        stream.close().await;
                        continue;
                    }
                }

                if let Some(peer_map) = connection_requests.get_mut(&peer_id) {
                    if let Some(rsp) = peer_map.remove(&connection_id) {
                        if handshake::inbound_ok(&mut stream).await.is_err() {
                            tracing::debug!("second handshake failed");
                        } else {
                            let _ = rsp.send(stream);
                            continue;
                        }
                    }
                }
                stream.close().await;
            }
            Err(e) => tracing::error!(%e, "accept_loop error"),
        }
    }
}
