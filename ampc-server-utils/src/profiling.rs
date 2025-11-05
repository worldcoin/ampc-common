//! Profiling endpoints for performance analysis

use axum::Router;

#[cfg(feature = "profiling")]
#[derive(Debug, serde::Deserialize)]
struct PprofQuery {
    seconds: Option<u64>,
    frequency: Option<i32>,
}

/// Returns router with pprof profiling endpoints.
///
/// When the `profiling` feature is enabled, provides:
/// - `/pprof/flame?seconds=30&frequency=99` - Returns an SVG flamegraph
/// - `/pprof/profile?seconds=30&frequency=99` - Returns a pprof protobuf profile
///
/// When the `profiling` feature is disabled, returns an empty router.
pub fn pprof_routes() -> Router {
    #[cfg(feature = "profiling")]
    {
        use axum::extract::Query;
        use axum::http::StatusCode;
        use axum::routing::get;
        use pprof::protos::Message;
        use pprof::ProfilerGuardBuilder;
        use std::time::Duration as StdDuration;
        use tokio::time::sleep as tokio_sleep;

        Router::new()
            .route(
                "/pprof/flame",
                get(|Query(q): Query<PprofQuery>| async move {
                    let seconds = q.seconds.unwrap_or(30).min(300);
                    let frequency = q.frequency.unwrap_or(99).clamp(1, 1000);
                    let guard = ProfilerGuardBuilder::default()
                        .frequency(frequency)
                        .build()
                        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                    tokio_sleep(StdDuration::from_secs(seconds)).await;
                    match guard.report().build() {
                        Ok(report) => {
                            let mut svg = Vec::new();
                            if let Err(e) = report.flamegraph(&mut svg) {
                                return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
                            }
                            Ok((StatusCode::OK, [("content-type", "image/svg+xml")], svg))
                        }
                        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                    }
                }),
            )
            .route(
                "/pprof/profile",
                get(|Query(q): Query<PprofQuery>| async move {
                    let seconds = q.seconds.unwrap_or(30).min(300);
                    let frequency = q.frequency.unwrap_or(99).clamp(1, 1000);
                    let guard = ProfilerGuardBuilder::default()
                        .frequency(frequency)
                        .build()
                        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                    tokio_sleep(StdDuration::from_secs(seconds)).await;
                    match guard.report().build() {
                        Ok(report) => match report.pprof() {
                            Ok(profile) => {
                                let mut buf = Vec::new();
                                if let Err(e) = profile.encode(&mut buf) {
                                    return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
                                }
                                Ok((
                                    StatusCode::OK,
                                    [("content-type", "application/octet-stream")],
                                    buf,
                                ))
                            }
                            Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                        },
                        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                    }
                }),
            )
    }

    #[cfg(not(feature = "profiling"))]
    {
        Router::new()
    }
}
