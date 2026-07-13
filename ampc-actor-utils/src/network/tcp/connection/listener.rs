use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tokio_rustls::rustls::{
    pki_types::{CertificateDer, UnixTime},
    server::WebPkiClientVerifier,
    RootCertStore,
};
use tokio_util::sync::CancellationToken;

use crate::{
    execution::player::Identity,
    network::tcp::{
        connection::handshake, ConnectionId, NetworkConnection, RuntimeTlsConfig, Server, TlsMode,
    },
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
    tls: Option<RuntimeTlsConfig>,
) {
    let mut connection_requests: HashMap<Identity, HashMap<ConnectionId, oneshot::Sender<T>>> =
        HashMap::new();

    // Validate that cert-pinning is only used with Mutual TLS
    if let Some(ref tls_cfg) = tls {
        if tls_cfg.validate_peer_ids && listener.tls_mode() != TlsMode::Mutual {
            tracing::warn!(
                tls_mode = ?listener.tls_mode(),
                "Peer ID validation is enabled but TLS mode is not Mutual. \
                 This will cause all connections to be rejected. Use TlsMode::Mutual."
            );
        }
    }

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

                // If enabled, verify that the peer's certificate chain is
                // anchored to the root CA expected for its claimed identity.
                if let Some(ref tls) = tls {
                    if tls.validate_peer_ids {
                        let identity = &peer_id.0;
                        let valid = tls
                            .peer_certs
                            .get(identity)
                            .map(|expected_root| {
                                stream
                                    .maybe_tls_cert_chain()
                                    .map(|chain| {
                                        is_chain_anchored_to_root(&chain, expected_root.as_slice())
                                    })
                                    .unwrap_or(false)
                            })
                            .unwrap_or(false);
                        if !valid {
                            tracing::warn!(
                                peer_id = identity,
                                "peer cert chain is not anchored to the expected root CA: \
                                 rejecting connection"
                            );
                            stream.close().await;
                            continue;
                        }
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

/// Verify that `cert_chain` (leaf cert first, then intermediates) is anchored
/// to `expected_root_der`. Returns `true` only when the chain can be fully
/// verified up to that specific root CA.
///
/// Because the root CA is not transmitted in a TLS handshake we cannot simply
/// compare cert bytes; instead we ask `WebPkiClientVerifier` to validate the
/// presented chain against the expected root, which also allows the peer to
/// rotate its leaf cert while the root CA remains stable.
fn is_chain_anchored_to_root(cert_chain: &[Vec<u8>], expected_root_der: &[u8]) -> bool {
    let mut root_store = RootCertStore::empty();
    if root_store
        .add(CertificateDer::from(expected_root_der.to_vec()))
        .is_err()
    {
        tracing::debug!("failed to add expected root cert to root store");
        return false;
    }
    let verifier = match WebPkiClientVerifier::builder(Arc::new(root_store)).build() {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("failed to build WebPkiClientVerifier: {e}");
            return false;
        }
    };
    let chain: Vec<CertificateDer<'_>> = cert_chain
        .iter()
        .map(|c| CertificateDer::from(c.as_slice()))
        .collect();
    let Some((end_entity, intermediates)) = chain.split_first() else {
        return false;
    };
    verifier
        .verify_client_cert(end_entity, intermediates, UnixTime::now())
        .is_ok()
}
