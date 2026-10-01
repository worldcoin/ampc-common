//! Shared Iris modifications persistence; callers own gallery transactions.
use super::{Modification, MOD_STATUS_IN_PROGRESS};
use eyre::Result;
use itertools::izip;
use sqlx::{PgConnection, PgPool};

#[derive(sqlx::FromRow, Debug, Default)]
pub struct StoredModification {
    pub id: i64,
    pub serial_id: Option<i64>,
    pub request_type: String,
    pub s3_url: Option<String>,
    pub status: String,
    pub persisted: bool,
    pub result_message_body: Option<String>,
}

impl From<StoredModification> for Modification {
    fn from(stored: StoredModification) -> Self {
        Self {
            id: stored.id,
            serial_id: stored.serial_id,
            request_type: stored.request_type,
            s3_url: stored.s3_url,
            status: stored.status,
            persisted: stored.persisted,
            result_message_body: stored.result_message_body,
        }
    }
}

pub async fn insert_modification<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    serial_id: Option<i64>,
    request_type: &str,
    s3_url: Option<&str>,
) -> Result<Modification> {
    let persisted = false;
    let inserted: StoredModification = sqlx::query_as::<_, StoredModification>(
        r#"
        INSERT INTO modifications (serial_id, request_type, s3_url, status, persisted)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING
            id,
            serial_id,
            request_type,
            s3_url,
            status,
            persisted,
            result_message_body
        "#,
    )
    .bind(serial_id)
    .bind(request_type)
    .bind(s3_url)
    .bind(MOD_STATUS_IN_PROGRESS)
    .bind(persisted)
    .fetch_one(executor)
    .await?;

    tracing::debug!(
        "Inserted {} modification: id={:?}, serial_id={:?}, request_type={}",
        MOD_STATUS_IN_PROGRESS,
        inserted.id,
        serial_id,
        request_type
    );

    Ok(inserted.into())
}

pub async fn last_modifications(pool: &PgPool, count: usize) -> Result<Vec<Modification>> {
    let rows = sqlx::query_as::<_, StoredModification>(
        r#"
        SELECT
            id,
            serial_id,
            request_type,
            s3_url,
            status,
            persisted,
            result_message_body
        FROM modifications
        ORDER BY id DESC
        LIMIT $1
        "#,
    )
    .bind(count as i64)
    .fetch_all(pool)
    .await?;

    let modifications = rows.into_iter().map(Into::into).collect();
    Ok(modifications)
}

pub async fn update_modifications(
    conn: &mut PgConnection,
    modifications: &[&Modification],
) -> Result<(), sqlx::Error> {
    if modifications.is_empty() {
        return Ok(());
    }

    let ids: Vec<i64> = modifications.iter().map(|m| m.id).collect();
    let statuses: Vec<String> = modifications.iter().map(|m| m.status.clone()).collect();
    let persisted: Vec<bool> = modifications.iter().map(|m| m.persisted).collect();
    let result_message_bodies: Vec<Option<String>> = modifications
        .iter()
        .map(|m| m.result_message_body.clone())
        .collect();
    let serial_ids: Vec<Option<i64>> = modifications.iter().map(|m| m.serial_id).collect();

    for (id, status, persisted, serial_id) in izip!(&ids, &statuses, &persisted, &serial_ids) {
        tracing::info!(
            "Updating modification id={} with status={}, persisted={}, serial_id={:?}",
            id,
            status,
            persisted,
            serial_id
        );
    }

    sqlx::query(
        r#"
        UPDATE modifications
        SET status = data.status,
            persisted = data.persisted,
            result_message_body = data.result_message_body,
            serial_id = data.serial_id
        FROM (
            SELECT
                unnest($1::bigint[])  as id,
                unnest($2::text[])    as status,
                unnest($3::bool[])    as persisted,
                unnest($4::text[])    as result_message_body,
                unnest($5::bigint[])  as serial_id
        ) as data
        WHERE modifications.id = data.id
        "#,
    )
    .bind(&ids)
    .bind(&statuses)
    .bind(&persisted)
    .bind(&result_message_bodies)
    .bind(&serial_ids)
    .execute(conn)
    .await?;

    Ok(())
}

pub async fn delete_modifications(
    conn: &mut PgConnection,
    modifications: &[Modification],
) -> Result<()> {
    if modifications.is_empty() {
        return Ok(());
    }

    // Extract the IDs from the modifications.
    let ids: Vec<i64> = modifications.iter().map(|m| m.id).collect();
    tracing::warn!(
        "Deleting modifications {:?} with IDs: {:?}",
        modifications,
        ids
    );

    // Execute a bulk delete using the ANY clause.
    sqlx::query(
        r#"
        DELETE FROM modifications
        WHERE id = ANY($1::bigint[])
        "#,
    )
    .bind(&ids)
    .execute(conn)
    .await?;

    Ok(())
}

pub async fn clear_modifications_table(conn: &mut PgConnection) -> Result<()> {
    sqlx::query("DELETE FROM modifications")
        .execute(conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Connection, Executor};

    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing to PostgreSQL"]
    async fn shared_crud_keeps_private_input_out_of_metadata_and_rolls_back() -> Result<()> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&std::env::var("DATABASE_URL")?)
            .await?;
        let mut conn = pool.acquire().await?;
        conn.execute("CREATE TEMP TABLE modifications (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, serial_id BIGINT, request_type TEXT NOT NULL, s3_url TEXT, status TEXT NOT NULL, persisted BOOLEAN NOT NULL DEFAULT FALSE, result_message_body TEXT, request BYTEA)").await?;
        let mut tx = conn.begin().await?;
        let mut row =
            insert_modification(&mut *tx, None, "uniqueness", Some("public-request-id")).await?;
        sqlx::query("UPDATE modifications SET request=$1 WHERE id=$2")
            .bind(b"private-shares".as_slice())
            .bind(row.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        drop(conn);
        let snapshot = last_modifications(&pool, 10).await?;
        assert_eq!(snapshot, vec![row.clone()]);
        assert!(!serde_json::to_string(&snapshot)?.contains("private-shares"));
        row.mark_completed(true, r#"{"node_id":0}"#, Some(42));
        let mut tx = pool.begin().await?;
        update_modifications(&mut tx, &[&row]).await?;
        tx.rollback().await?;
        assert_eq!(
            last_modifications(&pool, 10).await?[0].status,
            MOD_STATUS_IN_PROGRESS
        );
        let mut tx = pool.begin().await?;
        update_modifications(&mut tx, &[&row]).await?;
        tx.commit().await?;
        assert_eq!(last_modifications(&pool, 10).await?[0].serial_id, Some(42));
        assert_eq!(
            last_modifications(&pool, 10).await?[0].result_message_body,
            row.result_message_body
        );
        let private: Vec<u8> = sqlx::query_scalar("SELECT request FROM modifications WHERE id=$1")
            .bind(row.id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(private, b"private-shares");
        let mut tx = pool.begin().await?;
        delete_modifications(&mut tx, std::slice::from_ref(&row)).await?;
        tx.rollback().await?;
        assert_eq!(last_modifications(&pool, 10).await?.len(), 1);
        let mut tx = pool.begin().await?;
        delete_modifications(&mut tx, &[row]).await?;
        tx.commit().await?;
        assert!(last_modifications(&pool, 10).await?.is_empty());
        insert_modification(&pool, None, "uniqueness", Some("next-request-id")).await?;
        let mut conn = pool.acquire().await?;
        clear_modifications_table(&mut conn).await?;
        drop(conn);
        assert!(last_modifications(&pool, 10).await?.is_empty());
        pool.close().await;
        Ok(())
    }
}
