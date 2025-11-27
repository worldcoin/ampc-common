pub mod postgres;
use crate::{
    anon_stats::face::FaceDistance,
    store::postgres::{AccessMode, PostgresClient},
};
use eyre::Result;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};

use crate::types::{AnonStatsOperation, AnonStatsOrigin};

#[derive(Clone, Debug)]
pub struct AnonStatsStore {
    pub pool: PgPool,
    pub schema_name: String,
}

const ANON_STATS_1D_TABLE: &str = "anon_stats_1d";
const ANON_STATS_1D_LIFTED_TABLE: &str = "anon_stats_1d_lifted";
const ANON_STATS_2D_TABLE: &str = "anon_stats_2d";
const ANON_STATS_2D_LIFTED_TABLE: &str = "anon_stats_2d_lifted";
const ANON_STATS_FACE_TABLE: &str = "anon_stats_face";

impl AnonStatsStore {
    pub async fn new(postgres_client: &PostgresClient) -> Result<Self> {
        tracing::info!(
            "Created anon-stats-mpc-store with schema: {}",
            postgres_client.schema_name
        );

        if postgres_client.access_mode == AccessMode::ReadOnly {
            tracing::info!("Not migrating anon-stats-mpc-store DB in read-only mode");
        } else {
            sqlx::migrate!("./anon_stats_migrations/")
                .run(&postgres_client.pool)
                .await?;
        }

        Ok(AnonStatsStore {
            pool: postgres_client.pool.clone(),
            schema_name: postgres_client.schema_name.to_string(),
        })
    }

    pub async fn tx(&self) -> Result<Transaction<'_, Postgres>> {
        Ok(self.pool.begin().await?)
    }

    async fn num_available_anon_stats(
        &self,
        table_name: &'static str,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
    ) -> Result<i64> {
        let mut sql = format!(
            "SELECT COUNT(*) FROM {} WHERE processed = FALSE AND origin = $1",
            table_name
        );
        if operation.is_some() {
            sql.push_str(" AND operation = $2");
        }

        let mut query = sqlx::query_as::<_, (i64,)>(&sql).bind(i16::from(origin));
        if let Some(operation) = operation {
            query = query.bind(i16::from(operation));
        }

        let row = query.fetch_one(&self.pool).await?;
        Ok(row.0)
    }
    /// Get number of available lifted anon stats entries from the DB for the given origin.
    pub async fn num_available_anon_stats_1d(
        &self,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
    ) -> Result<i64> {
        self.num_available_anon_stats(ANON_STATS_1D_TABLE, origin, operation)
            .await
    }
    /// Get number of available lifted anon stats entries from the DB for the given origin.
    pub async fn num_available_anon_stats_1d_lifted(
        &self,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
    ) -> Result<i64> {
        self.num_available_anon_stats(ANON_STATS_1D_LIFTED_TABLE, origin, operation)
            .await
    }
    /// Get number of available lifted anon stats entries from the DB for the given origin.
    pub async fn num_available_anon_stats_2d(
        &self,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
    ) -> Result<i64> {
        self.num_available_anon_stats(ANON_STATS_2D_TABLE, origin, operation)
            .await
    }
    /// Get number of available lifted anon stats entries from the DB for the given origin.
    pub async fn num_available_anon_stats_2d_lifted(
        &self,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
    ) -> Result<i64> {
        self.num_available_anon_stats(ANON_STATS_2D_LIFTED_TABLE, origin, operation)
            .await
    }

    async fn get_available_anon_stats<T: for<'a> Deserialize<'a>>(
        &self,
        table_name: &'static str,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
        limit: usize,
    ) -> Result<(Vec<i64>, Vec<(i64, T)>)> {
        let mut sql = format!(
            "SELECT id, match_id, bundle FROM {} WHERE processed = FALSE and origin = $1",
            table_name
        );
        if operation.is_some() {
            sql.push_str(" AND operation = $2");
        }
        let limit_param = if operation.is_some() { "$3" } else { "$2" };
        sql.push_str(&format!(" ORDER BY id ASC LIMIT {}", limit_param));

        let mut query = sqlx::query_as::<_, (i64, i64, Vec<u8>)>(&sql).bind(i16::from(origin));
        if let Some(operation) = operation {
            query = query.bind(i16::from(operation));
        }
        query = query.bind(limit as i64);

        let res: Vec<(i64, i64, Vec<u8>)> = query.fetch_all(&self.pool).await?;

        let (ids, distance_bundles) = res
            .into_iter()
            .map(|(id, match_id, bundle_bytes)| {
                let bundle: T = bincode::deserialize(&bundle_bytes).map_err(|e| {
                    eyre::eyre!(
                        "Failed to deserialize distance bundle from table {} for anon_stats id {}: {:?}",
                        table_name,
                        id,
                        e
                    )
                })?;
                Result::<_, eyre::Report>::Ok((id, (match_id, bundle)))
            })
            .collect::<Result<(Vec<_>, Vec<_>), eyre::Report>>()?;

        Ok((ids, distance_bundles))
    }

    /// Get available anon stats entries from the DB for the given origin, up to the given limit.
    /// Returns a tuple of (ids, Vec<(match_id, T)>)
    pub async fn get_available_anon_stats_1d<T: for<'a> Deserialize<'a>>(
        &self,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
        limit: usize,
    ) -> Result<(Vec<i64>, Vec<(i64, T)>)> {
        self.get_available_anon_stats(ANON_STATS_1D_TABLE, origin, operation, limit)
            .await
    }
    /// Get available lifted anon stats entries from the DB for the given origin, up to the given limit.
    /// Returns a tuple of (ids, Vec<(match_id, T)>)
    pub async fn get_available_anon_stats_1d_lifted<T: for<'a> Deserialize<'a>>(
        &self,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
        limit: usize,
    ) -> Result<(Vec<i64>, Vec<(i64, T)>)> {
        self.get_available_anon_stats(ANON_STATS_1D_LIFTED_TABLE, origin, operation, limit)
            .await
    }
    /// Get available anon stats entries from the DB for the given origin, up to the given limit.
    /// Returns a tuple of (ids, Vec<(match_id, T)>)
    pub async fn get_available_anon_stats_2d<T: for<'a> Deserialize<'a>>(
        &self,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
        limit: usize,
    ) -> Result<(Vec<i64>, Vec<(i64, T)>)> {
        self.get_available_anon_stats(ANON_STATS_2D_TABLE, origin, operation, limit)
            .await
    }
    /// Get available lifted anon stats entries from the DB for the given origin, up to the given limit.
    /// Returns a tuple of (ids, Vec<(match_id, T)>)
    pub async fn get_available_anon_stats_2d_lifted<T: for<'a> Deserialize<'a>>(
        &self,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
        limit: usize,
    ) -> Result<(Vec<i64>, Vec<(i64, T)>)> {
        self.get_available_anon_stats(ANON_STATS_2D_LIFTED_TABLE, origin, operation, limit)
            .await
    }

    async fn mark_anon_stats_processed(&self, table_name: &'static str, ids: &[i64]) -> Result<()> {
        sqlx::query(
            &[
                "UPDATE ",
                table_name,
                r#" SET processed = TRUE WHERE id = ANY($1)
            "#,
            ]
            .concat(),
        )
        .bind(ids)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
    pub async fn mark_anon_stats_processed_1d(&self, ids: &[i64]) -> Result<()> {
        self.mark_anon_stats_processed(ANON_STATS_1D_TABLE, ids)
            .await
    }
    pub async fn mark_anon_stats_processed_1d_lifted(&self, ids: &[i64]) -> Result<()> {
        self.mark_anon_stats_processed(ANON_STATS_1D_LIFTED_TABLE, ids)
            .await
    }
    pub async fn mark_anon_stats_processed_2d(&self, ids: &[i64]) -> Result<()> {
        self.mark_anon_stats_processed(ANON_STATS_2D_TABLE, ids)
            .await
    }
    pub async fn mark_anon_stats_processed_2d_lifted(&self, ids: &[i64]) -> Result<()> {
        self.mark_anon_stats_processed(ANON_STATS_2D_LIFTED_TABLE, ids)
            .await
    }

    async fn clear_unprocessed_anon_stats(
        &self,
        table_name: &'static str,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
    ) -> Result<u64> {
        let mut sql = format!(
            "DELETE FROM {} WHERE processed = FALSE AND origin = $1",
            table_name
        );
        if operation.is_some() {
            sql.push_str(" AND operation = $2");
        }
        let mut query = sqlx::query(&sql).bind(i16::from(origin));
        if let Some(operation) = operation {
            query = query.bind(i16::from(operation));
        }
        let result = query.execute(&self.pool).await?;
        Ok(result.rows_affected())
    }

    pub async fn clear_unprocessed_anon_stats_1d(
        &self,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
    ) -> Result<u64> {
        self.clear_unprocessed_anon_stats(ANON_STATS_1D_TABLE, origin, operation)
            .await
    }

    pub async fn clear_unprocessed_anon_stats_1d_lifted(
        &self,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
    ) -> Result<u64> {
        self.clear_unprocessed_anon_stats(ANON_STATS_1D_LIFTED_TABLE, origin, operation)
            .await
    }

    pub async fn clear_unprocessed_anon_stats_2d(
        &self,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
    ) -> Result<u64> {
        self.clear_unprocessed_anon_stats(ANON_STATS_2D_TABLE, origin, operation)
            .await
    }

    pub async fn clear_unprocessed_anon_stats_2d_lifted(
        &self,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
    ) -> Result<u64> {
        self.clear_unprocessed_anon_stats(ANON_STATS_2D_LIFTED_TABLE, origin, operation)
            .await
    }

    const ANON_STATS_INSERT_BATCH_SIZE: usize = 10000;

    async fn insert_anon_stats_batch<T: Serialize>(
        &self,
        table_name: &'static str,
        anon_stats: &[(i64, T)],
        origin: AnonStatsOrigin,
        operation: AnonStatsOperation,
    ) -> Result<()> {
        tracing::info!(
            "Inserting {} anon stats into table {}",
            anon_stats.len(),
            table_name
        );

        if anon_stats.is_empty() {
            return Ok(());
        }
        let origin = i16::from(origin);
        let operation = i16::from(operation);
        let mut tx = self.pool.begin().await?;
        for chunk in anon_stats.chunks(Self::ANON_STATS_INSERT_BATCH_SIZE) {
            let mapped_chunk = chunk.iter().map(|(id, bundle)| {
                let bundle_bytes =
                    bincode::serialize(bundle).expect("Failed to serialize DistanceBundle");
                (id, bundle_bytes)
            });
            let mut query = sqlx::QueryBuilder::new(
                [
                    "INSERT INTO ",
                    table_name,
                    r#" (match_id, bundle, origin, operation)"#,
                ]
                .concat(),
            );
            query.push_values(mapped_chunk, |mut query, (id, bytes)| {
                query.push_bind(id);
                query.push_bind(bytes);
                query.push_bind(origin);
                query.push_bind(operation);
            });

            let res = query.build().execute(&mut *tx).await?;
            if res.rows_affected() != chunk.len() as u64 {
                return Err(eyre::eyre!(
                    "Expected to insert {} rows, but only inserted {} rows",
                    chunk.len(),
                    res.rows_affected()
                ));
            }
        }
        tx.commit().await?;

        Ok(())
    }

    pub async fn insert_anon_stats_batch_1d<T: Serialize>(
        &self,
        anon_stats: &[(i64, T)],
        origin: AnonStatsOrigin,
        operation: AnonStatsOperation,
    ) -> Result<()> {
        self.insert_anon_stats_batch(ANON_STATS_1D_TABLE, anon_stats, origin, operation)
            .await
    }
    pub async fn insert_anon_stats_batch_1d_lifted<T: Serialize>(
        &self,
        anon_stats: &[(i64, T)],
        origin: AnonStatsOrigin,
        operation: AnonStatsOperation,
    ) -> Result<()> {
        self.insert_anon_stats_batch(ANON_STATS_1D_LIFTED_TABLE, anon_stats, origin, operation)
            .await
    }
    pub async fn insert_anon_stats_batch_2d<T: Serialize>(
        &self,
        anon_stats: &[(i64, T)],
        origin: AnonStatsOrigin,
        operation: AnonStatsOperation,
    ) -> Result<()> {
        self.insert_anon_stats_batch(ANON_STATS_2D_TABLE, anon_stats, origin, operation)
            .await
    }
    pub async fn insert_anon_stats_batch_2d_lifted<T: Serialize>(
        &self,
        anon_stats: &[(i64, T)],
        origin: AnonStatsOrigin,
        operation: AnonStatsOperation,
    ) -> Result<()> {
        self.insert_anon_stats_batch(ANON_STATS_2D_LIFTED_TABLE, anon_stats, origin, operation)
            .await
    }

    // ------------------- FACE -------------------- //

    /// Insert face-ampc anon stats entries into the DB.
    /// In contrast to the other insert methods, this takes a large slice of FaceDistance and inserts it into a single row as a serialized bundle. It also saves the size of the bundle.
    /// The query id is saved as well, and used to sort the results when fetching them.
    pub async fn insert_anon_stats_face(
        &self,
        anon_stats: &[FaceDistance],
        query_id: i64,
        origin: AnonStatsOrigin,
    ) -> Result<()> {
        let len = anon_stats.len();
        tracing::info!("Inserting {len} anon stats into table 'anon_stats_face'",);
        if anon_stats.is_empty() {
            return Ok(());
        }
        let origin = i16::from(origin);
        let bundle_bytes =
            bincode::serialize(anon_stats).expect("Failed to serialize DistanceBundle");

        sqlx::query("INSERT INTO anon_stats_face (query_id, bundle, bundle_size, origin) VALUES ($1, $2, $3, $4)").bind(query_id).bind(bundle_bytes).bind(len as i64).bind(origin).execute(&self.pool).await?;

        Ok(())
    }

    /// Clear unprocessed face-ampc anon stats entries from the DB for the given origin.
    pub async fn clear_unprocessed_anon_stats_face(
        &self,
        origin: AnonStatsOrigin,
        operation: Option<AnonStatsOperation>,
    ) -> Result<u64> {
        self.clear_unprocessed_anon_stats(ANON_STATS_FACE_TABLE, origin, operation)
            .await
    }
    // Mark face-ampc anon stats entries as processed in the DB for the given ids.
    pub async fn mark_anon_stats_processed_face(&self, ids: &[i64]) -> Result<()> {
        self.mark_anon_stats_processed(ANON_STATS_FACE_TABLE, ids)
            .await
    }
    /// Get number of available face-ampc anon stats entries from the DB for the given origin.
    pub async fn num_available_anon_stats_face(&self, origin: AnonStatsOrigin) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(
            &[
                r#"SELECT COALESCE(SUM(bundle_size), 0) FROM "#,
                ANON_STATS_FACE_TABLE,
                r#" WHERE processed = FALSE and origin = $1
            "#,
            ]
            .concat(),
        )
        .bind(i16::from(origin))
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0)
    }

    /// Get available face-ampc anon stats entries from the DB for the given origin, up to the given limit.
    /// Returns a tuple of (ids, query_ids, Vec<FaceDistance>).
    pub async fn get_available_anon_stats_face(
        &self,
        origin: AnonStatsOrigin,
        limit: usize,
    ) -> Result<(Vec<i64>, Vec<i64>, Vec<FaceDistance>)> {
        let mut stream = sqlx::query_as(
            r#"
            SELECT id, query_id, bundle, bundle_size FROM anon_stats_face
            WHERE processed = FALSE and origin = $1
            ORDER BY query_id ASC
            "#,
        )
        .bind(i16::from(origin))
        .fetch(&self.pool);

        let mut ret = Vec::new();
        let mut ids = Vec::new();
        let mut query_ids = Vec::new();

        let mut fetched = 0;

        while let Some(row) = stream.try_next().await? {
            let (id, query_id, bundle_bytes, bundle_size): (i64, i64, Vec<u8>, i64) = row;
            let distances : Vec<FaceDistance> = bincode::deserialize(&bundle_bytes).map_err(|e| {
                eyre::eyre!(
                    "Failed to deserialize distance bundle from table {} for anon_stats id {}: {:?}",
                    ANON_STATS_FACE_TABLE,
                    id,
                    e
                )
            })?;
            fetched += bundle_size as usize;
            ret.extend(distances);
            query_ids.push(query_id);
            ids.push(id);
            if fetched >= limit {
                break;
            }
        }

        Ok((ids, query_ids, ret))
    }
}
