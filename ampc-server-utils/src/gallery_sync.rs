//! Public gallery metadata, exchanged freshly after recovery and after loading.
use crate::{config::ServerCoordinationConfig, server_coordination::try_get_endpoint_other_nodes};
use axum::{http::StatusCode, routing::get, Json, Router};
use eyre::{ensure, Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};

/// Structural gallery state. Never include party-local secret shares in the digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryState {
    pub db_len: u64,
    pub next_available_serial_id: i64,
    pub serial_ids_digest: [u8; 32],
}

/// Compare public metadata, identifying the conflicting party in the error.
pub fn ensure_gallery_consistency<'a>(
    local: &GalleryState,
    peers: impl IntoIterator<Item = (usize, &'a GalleryState)>,
) -> Result<()> {
    for (party, peer) in peers {
        ensure!(
            local == peer,
            "Gallery mismatch with party {party}: local={local:?}, peer={peer:?}"
        );
    }
    Ok(())
}

/// Fail if the database changed while the final gallery was being loaded.
pub fn ensure_gallery_unchanged(before: &GalleryState, after: &GalleryState) -> Result<()> {
    ensure!(
        before == after,
        "Gallery changed during startup loading: before={before:?}, after={after:?}"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub enum GalleryPhase {
    Recovered,
    Loaded,
}
impl GalleryPhase {
    fn index(self) -> usize {
        match self {
            Self::Recovered => 0,
            Self::Loaded => 1,
        }
    }
    fn endpoint(self) -> &'static str {
        match self {
            Self::Recovered => "gallery-sync/recovered",
            Self::Loaded => "gallery-sync/loaded",
        }
    }
}

/// Each startup phase retains its own snapshot so faster peers cannot overwrite an earlier phase.
#[derive(Clone, Default)]
pub struct GallerySync(Arc<RwLock<[Option<GalleryState>; 2]>>);

impl GallerySync {
    /// Merge these routes into the coordination server before startup barriers.
    pub fn routes(&self) -> Router {
        let mut routes = Router::new();
        for phase in [GalleryPhase::Recovered, GalleryPhase::Loaded] {
            let shared = self.clone();
            routes = routes.route(
                &format!("/{}", phase.endpoint()),
                get(move || {
                    let state =
                        shared.0.read().expect("gallery lock poisoned")[phase.index()].clone();
                    async move { state.map(Json).ok_or(StatusCode::SERVICE_UNAVAILABLE) }
                }),
            );
        }
        routes
    }

    /// Publish this phase's fresh local snapshot and wait for the same phase on every peer.
    pub async fn check(
        &self,
        config: &ServerCoordinationConfig,
        phase: GalleryPhase,
        local: GalleryState,
    ) -> Result<()> {
        self.0.write().expect("gallery lock poisoned")[phase.index()] = Some(local.clone());
        let responses = tokio::time::timeout(
            std::time::Duration::from_secs(config.startup_sync_timeout_secs),
            async {
                loop {
                    let responses = try_get_endpoint_other_nodes(config, phase.endpoint()).await?;
                    if responses
                        .iter()
                        .all(|(status, _)| *status != StatusCode::SERVICE_UNAVAILABLE)
                    {
                        return Ok::<_, eyre::Report>(responses);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(
                        config.http_query_retry_delay_ms,
                    ))
                    .await;
                }
            },
        )
        .await
        .wrap_err_with(|| {
            format!(
                "Timed out gathering {phase:?} gallery state for party {}",
                config.party_id
            )
        })??;
        // The coordination client returns peers in ascending party order, excluding this party.
        let parties = (0..3).filter(|party| *party != config.party_id);
        for (party, (status, body)) in parties.zip(responses) {
            ensure!(
                status.is_success(),
                "Party {party} returned {status} for {phase:?} gallery state"
            );
            let peer: GalleryState = serde_json::from_slice(&body)
                .wrap_err_with(|| format!("Invalid {phase:?} gallery state from party {party}"))?;
            ensure_gallery_consistency(&local, [(party, &peer)])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> GalleryState {
        GalleryState {
            db_len: 2,
            next_available_serial_id: 43,
            serial_ids_digest: [7; 32],
        }
    }
    #[test]
    fn sparse_and_empty_galleries_agree() {
        for local in [
            state(),
            GalleryState {
                db_len: 0,
                next_available_serial_id: 43,
                serial_ids_digest: [0; 32],
            },
        ] {
            assert!(ensure_gallery_consistency(&local, [(0, &local), (2, &local)]).is_ok());
            assert!(ensure_gallery_unchanged(&local, &local).is_ok());
        }
    }
    #[test]
    fn mismatches_identify_party_and_state_and_detect_load_changes() {
        let local = state();
        let mut peers = [state(), state(), state()];
        peers[0].db_len += 1;
        peers[1].next_available_serial_id += 1;
        peers[2].serial_ids_digest[0] += 1;
        for peer in peers {
            let error = ensure_gallery_consistency(&local, [(2, &peer)])
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("party 2") && error.contains("local=") && error.contains("peer=")
            );
            assert!(ensure_gallery_unchanged(&local, &peer).is_err());
        }
    }
    #[test]
    fn phases_do_not_overwrite_each_other() {
        let sync = GallerySync::default();
        sync.0.write().unwrap()[0] = Some(state());
        let mut loaded = state();
        loaded.db_len += 1;
        sync.0.write().unwrap()[1] = Some(loaded);
        assert_eq!(sync.0.read().unwrap()[0], Some(state()));
    }

    #[tokio::test]
    async fn exchanges_fresh_phases_and_reports_conflicting_party() {
        let syncs = [
            GallerySync::default(),
            GallerySync::default(),
            GallerySync::default(),
        ];
        let mut servers = Vec::new();
        let mut ports = Vec::new();
        for sync in &syncs {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            ports.push(listener.local_addr().unwrap().port().to_string());
            let routes = sync.routes();
            servers.push(tokio::spawn(async move {
                axum::serve(listener, routes).await.unwrap()
            }));
        }
        let configs: Vec<ServerCoordinationConfig> = (0..3).map(|party| serde_json::from_value(serde_json::json!({
                "party_id": party, "node_hostnames": "[\"127.0.0.1\", \"127.0.0.1\", \"127.0.0.1\"]",
                "healthcheck_ports": serde_json::to_string(&ports).unwrap(), "http_query_retry_delay_ms": 1, "startup_sync_timeout_secs": 5
        })).unwrap()).collect();
        let (a, b, c) = tokio::join!(
            syncs[0].check(&configs[0], GalleryPhase::Recovered, state()),
            syncs[1].check(&configs[1], GalleryPhase::Recovered, state()),
            syncs[2].check(&configs[2], GalleryPhase::Recovered, state()),
        );
        a.unwrap();
        b.unwrap();
        c.unwrap();
        let mut changed = state();
        changed.serial_ids_digest[0] += 1;
        let delayed = async {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            syncs[2]
                .check(&configs[2], GalleryPhase::Loaded, changed)
                .await
        };
        let (a, b, c) = tokio::join!(
            syncs[0].check(&configs[0], GalleryPhase::Loaded, state()),
            syncs[1].check(&configs[1], GalleryPhase::Loaded, state()),
            delayed,
        );
        assert!(a.unwrap_err().to_string().contains("party 2"));
        assert!(b.unwrap_err().to_string().contains("party 2"));
        assert!(c.is_err());
        for server in servers {
            server.abort();
        }
    }
}
