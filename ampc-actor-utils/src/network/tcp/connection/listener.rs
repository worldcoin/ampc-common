use std::collections::HashMap;
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
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
        if !tls_cfg.validate_peer_ids && listener.tls_mode() != TlsMode::Mutual {
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

                // If enabled, verify the peer presented the
                // expected certificate for its claimed identity.
                if let Some(ref tls) = tls {
                    if tls.validate_peer_ids {
                        let identity = &peer_id.0;
                        let valid = tls
                            .peer_certs
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
