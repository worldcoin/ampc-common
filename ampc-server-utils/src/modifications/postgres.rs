//! Shared Iris modifications persistence; callers own gallery transactions.
use super::{
    Modification, ModificationInput, ModificationInputReference, ModificationInputStorage,
    MOD_STATUS_IN_PROGRESS,
};
use eyre::{Context, Result};
use itertools::izip;
use sqlx::{PgConnection, PgPool};

#[derive(sqlx::FromRow, Debug, Default)]
pub struct StoredModification {
    pub id: i64,
    pub serial_id: Option<i64>,
    pub request_type: String,
    pub input_reference: Option<String>,
    pub status: String,
    pub persisted: bool,
    pub result_message_body: Option<String>,
}

impl StoredModification {
    pub fn into_modification(self, storage: ModificationInputStorage) -> Modification {
        Modification {
            id: self.id,
            serial_id: self.serial_id,
            request_type: self.request_type,
            input: self.input_reference.map(|reference| match storage {
                ModificationInputStorage::S3 => ModificationInputReference::S3(reference),
                ModificationInputStorage::Inline => ModificationInputReference::Inline(reference),
            }),
            status: self.status,
            persisted: self.persisted,
            result_message_body: self.result_message_body,
        }
    }
}

pub async fn insert_modification<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    serial_id: Option<i64>,
    request_type: &str,
    input: Option<&ModificationInput>,
) -> Result<Modification> {
    let (inserted, storage) = match input {
        Some(ModificationInput::Inline { reference, bytes }) => {
            let row = sqlx::query_as::<_, StoredModification>(
                r#"
        INSERT INTO modifications (serial_id, request_type, input_reference, request, status, persisted)
        VALUES ($1, $2, $3, $4, $5, FALSE)
        RETURNING id, serial_id, request_type, input_reference, status, persisted, result_message_body
        "#,
            )
            .bind(serial_id)
            .bind(request_type)
            .bind(reference)
            .bind(bytes)
            .bind(MOD_STATUS_IN_PROGRESS)
            .fetch_one(executor)
            .await?;
            (row, ModificationInputStorage::Inline)
        }
        Some(ModificationInput::S3(_)) | None => {
            let reference = match input {
                Some(ModificationInput::S3(reference)) => Some(reference.as_str()),
                _ => None,
            };
            let row = sqlx::query_as::<_, StoredModification>(
                r#"
        INSERT INTO modifications (serial_id, request_type, s3_url, status, persisted)
        VALUES ($1, $2, $3, $4, FALSE)
        RETURNING
            id,
            serial_id,
            request_type,
            s3_url AS input_reference,
            status,
            persisted,
            result_message_body
        "#,
            )
            .bind(serial_id)
            .bind(request_type)
            .bind(reference)
            .bind(MOD_STATUS_IN_PROGRESS)
            .fetch_one(executor)
            .await?;
            (row, ModificationInputStorage::S3)
        }
    };

    tracing::debug!(
        "Inserted {} modification: id={:?}, serial_id={:?}, request_type={}",
        MOD_STATUS_IN_PROGRESS,
        inserted.id,
        serial_id,
        request_type
    );

    Ok(inserted.into_modification(storage))
}

pub async fn last_modifications(
    pool: &PgPool,
    count: usize,
    storage: ModificationInputStorage,
) -> Result<Vec<Modification>> {
    let query = match storage {
        ModificationInputStorage::S3 => {
            r#"
        SELECT
            id,
            serial_id,
            request_type,
            s3_url AS input_reference,
            status,
            persisted,
            result_message_body
        FROM modifications
        ORDER BY id DESC
        LIMIT $1
        "#
        }
        ModificationInputStorage::Inline => {
            r#"
        SELECT id, serial_id, request_type, input_reference, status, persisted, result_message_body
        FROM modifications
        ORDER BY id DESC
        LIMIT $1
        "#
        }
    };
    let rows = sqlx::query_as::<_, StoredModification>(query)
        .bind(count as i64)
        .fetch_all(pool)
        .await?;

    let modifications = rows
        .into_iter()
        .map(|row| row.into_modification(storage))
        .collect();
    Ok(modifications)
}

pub async fn load_modification_input<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    modification: &Modification,
) -> Result<ModificationInput> {
    match modification.input.as_ref() {
        Some(ModificationInputReference::S3(reference)) => {
            Ok(ModificationInput::S3(reference.clone()))
        }
        Some(ModificationInputReference::Inline(reference)) => {
            let bytes: Option<Vec<u8>> = sqlx::query_scalar(
                "SELECT request FROM modifications WHERE id=$1 AND input_reference=$2",
            )
            .bind(modification.id)
            .bind(reference)
            .fetch_one(executor)
            .await
            .wrap_err_with(|| {
                format!("Loading local input for modification {}", modification.id)
            })?;
            Ok(ModificationInput::Inline {
                reference: reference.clone(),
                bytes: bytes.ok_or_else(|| {
                    eyre::eyre!("Missing local input for modification {}", modification.id)
                })?,
            })
        }
        None => eyre::bail!(
            "Missing input reference for modification {}",
            modification.id
        ),
    }
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
    async fn s3_crud_preserves_the_existing_schema() -> Result<()> {
        shared_crud(ModificationInputStorage::S3).await
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing to PostgreSQL"]
    async fn inline_crud_keeps_private_input_local() -> Result<()> {
        shared_crud(ModificationInputStorage::Inline).await
    }

    async fn shared_crud(storage: ModificationInputStorage) -> Result<()> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&std::env::var("DATABASE_URL")?)
            .await?;
        let mut conn = pool.acquire().await?;
        let schema = match storage {
            ModificationInputStorage::S3 => "CREATE TEMP TABLE modifications (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, serial_id BIGINT, request_type TEXT NOT NULL, s3_url TEXT, status TEXT NOT NULL, persisted BOOLEAN NOT NULL DEFAULT FALSE, result_message_body TEXT)",
            ModificationInputStorage::Inline => "CREATE TEMP TABLE modifications (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, serial_id BIGINT, request_type TEXT NOT NULL, input_reference TEXT, status TEXT NOT NULL, persisted BOOLEAN NOT NULL DEFAULT FALSE, result_message_body TEXT, request BYTEA)",
        };
        conn.execute(schema).await?;
        let input = match storage {
            ModificationInputStorage::S3 => ModificationInput::S3("public-request-id".into()),
            ModificationInputStorage::Inline => ModificationInput::Inline {
                reference: "public-request-id".into(),
                bytes: b"private-shares".to_vec(),
            },
        };
        let mut tx = conn.begin().await?;
        let mut row = insert_modification(&mut *tx, None, "uniqueness", Some(&input)).await?;
        tx.commit().await?;
        drop(conn);
        let snapshot = last_modifications(&pool, 10, storage).await?;
        assert_eq!(snapshot, vec![row.clone()]);
        assert!(!serde_json::to_string(&snapshot)?.contains("private-shares"));
        let loaded = load_modification_input(&pool, &row).await?;
        match loaded {
            ModificationInput::S3(reference) => assert_eq!(reference, "public-request-id"),
            ModificationInput::Inline { reference, bytes } => {
                assert_eq!(reference, "public-request-id");
                assert_eq!(bytes, b"private-shares");
                let mut mismatched = row.clone();
                mismatched.input = Some(ModificationInputReference::Inline(
                    "another-request-id".into(),
                ));
                assert!(load_modification_input(&pool, &mismatched).await.is_err());
                mismatched.input = row.input.clone();
                mismatched.id += 1;
                assert!(load_modification_input(&pool, &mismatched).await.is_err());
                sqlx::query("UPDATE modifications SET request=NULL WHERE id=$1")
                    .bind(row.id)
                    .execute(&pool)
                    .await?;
                assert!(load_modification_input(&pool, &row).await.is_err());
                sqlx::query("UPDATE modifications SET request=$1 WHERE id=$2")
                    .bind(bytes)
                    .bind(row.id)
                    .execute(&pool)
                    .await?;
            }
        }
        let mut missing = row.clone();
        missing.input = None;
        assert!(load_modification_input(&pool, &missing).await.is_err());
        let mut tx = pool.begin().await?;
        insert_modification(&mut *tx, None, "uniqueness", Some(&input)).await?;
        tx.rollback().await?;
        assert_eq!(last_modifications(&pool, 10, storage).await?.len(), 1);
        row.mark_completed(true, r#"{"node_id":0}"#, Some(42));
        let mut tx = pool.begin().await?;
        update_modifications(&mut tx, &[&row]).await?;
        tx.rollback().await?;
        assert_eq!(
            last_modifications(&pool, 10, storage).await?[0].status,
            MOD_STATUS_IN_PROGRESS
        );
        let mut tx = pool.begin().await?;
        update_modifications(&mut tx, &[&row]).await?;
        tx.commit().await?;
        assert_eq!(
            last_modifications(&pool, 10, storage).await?[0].serial_id,
            Some(42)
        );
        assert_eq!(
            last_modifications(&pool, 10, storage).await?[0].result_message_body,
            row.result_message_body
        );
        let mut tx = pool.begin().await?;
        delete_modifications(&mut tx, std::slice::from_ref(&row)).await?;
        tx.rollback().await?;
        assert_eq!(last_modifications(&pool, 10, storage).await?.len(), 1);
        let mut tx = pool.begin().await?;
        delete_modifications(&mut tx, &[row]).await?;
        tx.commit().await?;
        assert!(last_modifications(&pool, 10, storage).await?.is_empty());
        insert_modification(&pool, None, "uniqueness", Some(&input)).await?;
        let mut conn = pool.acquire().await?;
        clear_modifications_table(&mut conn).await?;
        drop(conn);
        assert!(last_modifications(&pool, 10, storage).await?.is_empty());
        if matches!(storage, ModificationInputStorage::S3) {
            let deletion = insert_modification(&pool, Some(42), "identity_deletion", None).await?;
            assert_eq!(deletion.input, None);
            assert_eq!(
                last_modifications(&pool, 10, storage).await?,
                vec![deletion]
            );
        }
        pool.close().await;
        Ok(())
    }
}
