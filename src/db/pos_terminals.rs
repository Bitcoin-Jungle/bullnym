use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, sqlx::FromRow)]
pub struct PosTerminal {
    pub id: Uuid,
    pub nym: String,
    pub npub_owner: Option<String>,
    pub label: Option<String>,
    pub token_hash: String,
    pub pairing_code_hash: Option<String>,
    pub pairing_expires_at_unix: i64,
    pub claimed_at_unix: Option<i64>,
    pub last_seen_at_unix: Option<i64>,
    pub revoked_at_unix: Option<i64>,
    pub created_at_unix: i64,
}

const POS_TERMINAL_COLUMNS: &str = "id, nym, npub_owner, label, token_hash, pairing_code_hash, \
     EXTRACT(EPOCH FROM pairing_expires_at)::BIGINT AS pairing_expires_at_unix, \
     EXTRACT(EPOCH FROM claimed_at)::BIGINT         AS claimed_at_unix, \
     EXTRACT(EPOCH FROM last_seen_at)::BIGINT       AS last_seen_at_unix, \
     EXTRACT(EPOCH FROM revoked_at)::BIGINT         AS revoked_at_unix, \
     EXTRACT(EPOCH FROM created_at)::BIGINT         AS created_at_unix";

pub async fn insert_pos_pairing(
    pool: &PgPool,
    nym: &str,
    token_hash: &str,
    pairing_code_hash: &str,
    pairing_expires_in_secs: i64,
) -> Result<PosTerminal, sqlx::Error> {
    sqlx::query_as::<_, PosTerminal>(&format!(
        "INSERT INTO pos_terminals \
            (nym, token_hash, pairing_code_hash, pairing_expires_at) \
         VALUES ($1, $2, $3, NOW() + ($4 || ' seconds')::interval) \
         RETURNING {POS_TERMINAL_COLUMNS}"
    ))
    .bind(nym)
    .bind(token_hash)
    .bind(pairing_code_hash)
    .bind(pairing_expires_in_secs)
    .fetch_one(pool)
    .await
}

pub async fn claim_pos_pairing(
    pool: &PgPool,
    nym: &str,
    pairing_code_hash: &str,
    npub_owner: &str,
    label: Option<&str>,
    pos_ct_descriptor: &str,
) -> Result<Option<PosTerminal>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let pending = sqlx::query_as::<_, PosTerminal>(&format!(
        "SELECT {POS_TERMINAL_COLUMNS} \
         FROM pos_terminals \
         WHERE nym = $1 \
           AND pairing_code_hash = $2 \
           AND claimed_at IS NULL \
           AND pairing_expires_at > NOW() \
         FOR UPDATE"
    ))
    .bind(nym)
    .bind(pairing_code_hash)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(pending) = pending else {
        tx.commit().await?;
        return Ok(None);
    };

    // POS is nym-gated: when the merchant never configured a donation page,
    // the claim materializes a disabled placeholder row to store the POS
    // descriptor. ON CONFLICT keeps an existing page untouched; a concurrent
    // claim blocks on the speculative insert and then serializes on the
    // SELECT ... FOR UPDATE below, so two claims can never store two
    // different descriptors.
    sqlx::query(
        "INSERT INTO donation_pages \
            (nym, header, description, display_currency, enabled) \
         VALUES ($1, $1, '', 'USD', FALSE) \
         ON CONFLICT (nym) DO NOTHING",
    )
    .bind(nym)
    .execute(&mut *tx)
    .await?;

    let existing_pos_descriptor: Option<String> = sqlx::query_scalar(
        "SELECT pos_ct_descriptor \
         FROM donation_pages \
         WHERE nym = $1 \
         FOR UPDATE",
    )
    .bind(nym)
    .fetch_one(&mut *tx)
    .await?;

    match existing_pos_descriptor.as_deref() {
        Some(existing) if existing != pos_ct_descriptor => {
            return Err(sqlx::Error::Protocol(
                "POS descriptor mismatch for nym".to_string(),
            ));
        }
        Some(_) => {}
        None => {
            sqlx::query(
                "UPDATE donation_pages \
                 SET pos_ct_descriptor = $2 \
                 WHERE nym = $1",
            )
            .bind(nym)
            .bind(pos_ct_descriptor)
            .execute(&mut *tx)
            .await?;
        }
    }

    let updated = sqlx::query_as::<_, PosTerminal>(&format!(
        "UPDATE pos_terminals \
         SET npub_owner = $2, label = $3, claimed_at = NOW(), pairing_code_hash = NULL \
         WHERE id = $1 \
         RETURNING {POS_TERMINAL_COLUMNS}"
    ))
    .bind(pending.id)
    .bind(npub_owner)
    .bind(label)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(updated)
}

pub async fn get_pos_pairing(
    pool: &PgPool,
    id: Uuid,
    token_hash: &str,
) -> Result<Option<PosTerminal>, sqlx::Error> {
    sqlx::query_as::<_, PosTerminal>(&format!(
        "SELECT {POS_TERMINAL_COLUMNS} \
         FROM pos_terminals \
         WHERE id = $1 AND token_hash = $2"
    ))
    .bind(id)
    .bind(token_hash)
    .fetch_optional(pool)
    .await
}

pub async fn get_active_terminal_by_token(
    pool: &PgPool,
    nym: &str,
    token_hash: &str,
) -> Result<Option<PosTerminal>, sqlx::Error> {
    sqlx::query_as::<_, PosTerminal>(&format!(
        "SELECT {POS_TERMINAL_COLUMNS} \
         FROM pos_terminals \
         WHERE nym = $1 \
           AND token_hash = $2 \
           AND claimed_at IS NOT NULL \
           AND revoked_at IS NULL"
    ))
    .bind(nym)
    .bind(token_hash)
    .fetch_optional(pool)
    .await
}

pub async fn touch_terminal_seen(pool: &PgPool, id: Uuid) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE pos_terminals \
         SET last_seen_at = NOW() \
         WHERE id = $1 \
           AND revoked_at IS NULL \
           AND (last_seen_at IS NULL OR last_seen_at < NOW() - INTERVAL '60 seconds')",
    )
    .bind(id)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
}

pub async fn list_terminals_by_npub(
    pool: &PgPool,
    npub_owner: &str,
) -> Result<Vec<PosTerminal>, sqlx::Error> {
    sqlx::query_as::<_, PosTerminal>(&format!(
        "SELECT {POS_TERMINAL_COLUMNS} \
         FROM pos_terminals \
         WHERE npub_owner = $1 \
         ORDER BY created_at DESC"
    ))
    .bind(npub_owner)
    .fetch_all(pool)
    .await
}

pub async fn revoke_terminal(
    pool: &PgPool,
    id: Uuid,
    npub_owner: &str,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE pos_terminals \
         SET revoked_at = COALESCE(revoked_at, NOW()) \
         WHERE id = $1 AND npub_owner = $2 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(npub_owner)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
}

pub async fn purge_expired_unclaimed_terminals(
    pool: &PgPool,
    grace_secs: u64,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "DELETE FROM pos_terminals \
         WHERE claimed_at IS NULL \
           AND pairing_expires_at < NOW() - ($1 || ' seconds')::interval",
    )
    .bind(grace_secs as i64)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
}
