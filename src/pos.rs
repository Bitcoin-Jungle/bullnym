use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{header, HeaderMap};
use axum::Json;
use secp256k1::rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::auth;
use crate::db;
use crate::descriptor;
use crate::error::AppError;
use crate::invoice;
use crate::ip_whitelist;
use crate::AppState;

pub const ACTION_PAIR: &str = "pos-pair";
pub const ACTION_TERMINAL_LIST: &str = "pos-terminal-list";
pub const ACTION_TERMINAL_REVOKE: &str = "pos-terminal-revoke";
pub const PAIRING_CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

const PAIRING_CODE_LEN: usize = 8;
const PAIRING_EXPIRES_SECS: i64 = 5 * 60;

#[derive(Deserialize)]
pub struct CreatePairingRequest {
    pub token_hash: String,
}

#[derive(Serialize)]
pub struct CreatePairingResponse {
    pub pairing_id: Uuid,
    pub code: String,
    pub expires_at_unix: i64,
}

pub async fn create_pairing(
    State(state): State<AppState>,
    Path(nym): Path<String>,
    peer_opt: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(req): Json<CreatePairingRequest>,
) -> Result<Json<CreatePairingResponse>, AppError> {
    if !is_hex64(&req.token_hash) {
        return Err(AppError::InvalidAmount(
            "token_hash must be 64 lowercase hex chars".into(),
        ));
    }
    check_ip_rate_limit(&state, peer_opt, &headers, PosRateBucket::Create).await?;

    // POS is nym-gated, not page-gated: a merchant can pair a terminal
    // before ever configuring a donation page. An archived page still
    // blocks POS everywhere. The error code stays `DonationPageNotFound`
    // for wire compatibility with deployed terminals.
    let user = db::get_active_user_by_nym(&state.db, &nym)
        .await?
        .ok_or_else(|| AppError::DonationPageNotFound(nym.clone()))?;
    if let Some(page) = db::get_donation_page_by_nym(&state.db, &nym).await? {
        if page.is_archived {
            return Err(AppError::DonationPageNotFound(nym));
        }
    }

    let code = generate_pairing_code();
    let code_hash = sha256_hex(&code);
    let row = db::insert_pos_pairing(
        &state.db,
        &user.nym,
        &req.token_hash,
        &code_hash,
        PAIRING_EXPIRES_SECS,
    )
    .await?;

    Ok(Json(CreatePairingResponse {
        pairing_id: row.id,
        code,
        expires_at_unix: row.pairing_expires_at_unix,
    }))
}

#[derive(Deserialize)]
pub struct ClaimPairingRequest {
    pub npub: String,
    pub nym: String,
    pub code: String,
    pub label: Option<String>,
    pub pos_ct_descriptor: String,
    pub timestamp: u64,
    pub signature: String,
}

#[derive(Serialize)]
pub struct ClaimPairingResponse {
    pub terminal_id: Uuid,
    pub nym: String,
    pub label: Option<String>,
    pub claimed_at_unix: i64,
}

pub async fn claim_pairing(
    State(state): State<AppState>,
    peer_opt: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(req): Json<ClaimPairingRequest>,
) -> Result<Json<ClaimPairingResponse>, AppError> {
    let peer = peer_opt.map(|ConnectInfo(addr)| addr);
    let ip = ip_whitelist::caller_ip(peer, &headers, state.config.rate_limit.trust_forwarded_for);
    let is_whitelisted = ip
        .map(|ip| state.ip_whitelist.contains(ip))
        .unwrap_or(false);

    if !is_whitelisted {
        if let Some(ip) = ip {
            state
                .rate_limiter
                .check_pos_pairing_claim_failure_per_source(ip)
                .await?;
        }
    }

    let label_for_sig = req.label.as_deref().unwrap_or("");
    auth::verify_la_v2(
        ACTION_PAIR,
        &req.npub,
        &req.nym,
        &[&req.code, label_for_sig, &req.pos_ct_descriptor],
        req.timestamp,
        &req.signature,
    )?;
    if req.pos_ct_descriptor.is_empty() {
        return Err(AppError::InvalidDescriptor(
            "POS descriptor is required".into(),
        ));
    }
    descriptor::validate_descriptor(
        &req.pos_ct_descriptor,
        state.config.limits.max_descriptor_len,
    )?;
    invoice::assert_nym_owner(&state, &req.nym, &req.npub).await?;
    // Archived pages block POS everywhere, including claims for pairings
    // created just before the archive.
    if let Some(page) = db::get_donation_page_by_nym(&state.db, &req.nym).await? {
        if page.is_archived {
            return Err(pairing_not_found(&req.nym));
        }
    }
    let label = validate_label(req.label.as_deref())?;

    let code_hash = sha256_hex(&req.code);
    let claimed = db::claim_pos_pairing(
        &state.db,
        &req.nym,
        &code_hash,
        &req.npub,
        label.as_deref(),
        &req.pos_ct_descriptor,
    )
    .await
    .map_err(|e| match &e {
        sqlx::Error::Protocol(msg) if msg.contains("POS descriptor mismatch") => {
            AppError::PosDescriptorMismatch(req.nym.clone())
        }
        _ => AppError::from(e),
    })?;

    let Some(row) = claimed else {
        if !is_whitelisted {
            if let Some(ip) = ip {
                state
                    .rate_limiter
                    .record_pos_pairing_claim_failure_per_source(ip)
                    .await;
            }
        }
        return Err(pairing_not_found(&req.nym));
    };

    Ok(Json(ClaimPairingResponse {
        terminal_id: row.id,
        nym: row.nym,
        label: row.label,
        claimed_at_unix: row.claimed_at_unix.unwrap_or(0),
    }))
}

#[derive(Deserialize)]
pub struct PollPairingQuery {
    pub token_hash: Option<String>,
}

#[derive(Serialize)]
pub struct PollPairingResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<Uuid>,
}

pub async fn poll_pairing(
    State(state): State<AppState>,
    Path((nym, id)): Path<(String, Uuid)>,
    Query(query): Query<PollPairingQuery>,
    peer_opt: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
) -> Result<Json<PollPairingResponse>, AppError> {
    check_ip_rate_limit(&state, peer_opt, &headers, PosRateBucket::Poll).await?;
    let token_hash = query
        .token_hash
        .as_deref()
        .filter(|s| is_hex64(s))
        .ok_or_else(|| pairing_not_found(&nym))?;
    let row = db::get_pos_pairing(&state.db, id, token_hash)
        .await?
        .ok_or_else(|| pairing_not_found(&nym))?;
    if row.nym != nym {
        return Err(pairing_not_found(&nym));
    }
    if row.claimed_at_unix.is_some() {
        return Ok(Json(PollPairingResponse {
            status: "approved".into(),
            terminal_id: Some(row.id),
        }));
    }
    let status = if row.pairing_expires_at_unix <= unix_now() {
        "expired"
    } else {
        "pending"
    };
    Ok(Json(PollPairingResponse {
        status: status.into(),
        terminal_id: None,
    }))
}

pub(crate) async fn authenticate_terminal(
    state: &AppState,
    nym: &str,
    headers: &HeaderMap,
) -> Result<db::PosTerminal, AppError> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::AuthError("missing bearer token".into()))?;
    let token = raw
        .strip_prefix("Bearer ")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::AuthError("missing bearer token".into()))?;
    let token_hash = sha256_hex(token);
    let terminal = db::get_active_terminal_by_token(&state.db, nym, &token_hash)
        .await?
        .ok_or_else(|| AppError::AuthError("invalid terminal token".into()))?;
    db::touch_terminal_seen(&state.db, terminal.id).await?;
    Ok(terminal)
}

#[derive(Deserialize)]
pub struct CreateTerminalInvoiceRequest {
    pub amount_sat: Option<i64>,
    pub fiat_amount_minor: Option<i32>,
    pub fiat_currency: Option<String>,
    pub memo: Option<String>,
}

pub async fn create_invoice(
    State(state): State<AppState>,
    Path(nym): Path<String>,
    peer_opt: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(req): Json<CreateTerminalInvoiceRequest>,
) -> Result<Json<invoice::CreateInvoiceResponse>, AppError> {
    let terminal = authenticate_terminal(&state, &nym, &headers).await?;
    state
        .rate_limiter
        .check_pos_invoice_create_per_terminal(terminal.id)
        .await?;
    let memo = invoice::validate_pos_memo(req.memo.as_deref())?;
    invoice::create_checkout_invoice_inner(
        &state,
        nym,
        peer_opt,
        headers,
        invoice::CreateAnonymousRequest {
            amount_sat: req.amount_sat,
            fiat_amount_minor: req.fiat_amount_minor,
            fiat_currency: req.fiat_currency,
        },
        invoice::CheckoutInvoiceOptions {
            terminal_id: Some(terminal.id),
            memo,
        },
    )
    .await
}

#[derive(Deserialize)]
pub struct ListInvoicesQuery {
    pub page: i64,
    #[serde(rename = "pageSize")]
    pub page_size: i64,
    pub status: Option<String>,
}

#[derive(Serialize)]
pub struct PosInvoiceListItem {
    pub id: Uuid,
    pub status: String,
    pub pricing_mode: String,
    pub settlement_status: String,
    pub amount_sat: i64,
    pub remaining_amount_sat: i64,
    pub fiat_amount_minor: Option<i32>,
    pub fiat_currency: Option<String>,
    pub memo: Option<String>,
    pub terminal_id: Option<Uuid>,
    pub accept_btc: bool,
    pub accept_ln: bool,
    pub accept_liquid: bool,
    pub bitcoin_address: Option<String>,
    pub liquid_address: Option<String>,
    pub created_at_unix: i64,
    pub expires_at_unix: i64,
    pub paid_via: Option<String>,
    pub paid_at_unix: Option<i64>,
    pub paid_amount_sat: Option<i64>,
}

#[derive(Serialize)]
pub struct ListInvoicesResponse {
    pub invoices: Vec<PosInvoiceListItem>,
    pub page: i64,
    #[serde(rename = "pageSize")]
    pub page_size: i64,
    pub has_more: bool,
}

pub async fn list_invoices(
    State(state): State<AppState>,
    Path(nym): Path<String>,
    headers: HeaderMap,
    Query(params): Query<ListInvoicesQuery>,
) -> Result<Json<ListInvoicesResponse>, AppError> {
    authenticate_terminal(&state, &nym, &headers).await?;
    let (page, page_size, status_filter) =
        validate_list_query(params.page, params.page_size, params.status.as_deref())?;
    let rows =
        db::list_pos_invoices_by_nym(&state.db, &nym, status_filter, page, page_size).await?;
    let has_more = rows.len() >= page_size as usize;
    let invoices = rows
        .into_iter()
        .map(|inv| {
            let remaining = invoice::remaining_amount_sat(&inv);
            PosInvoiceListItem {
                id: inv.id,
                status: inv.status,
                pricing_mode: inv.pricing_mode,
                settlement_status: inv.settlement_status,
                amount_sat: inv.amount_sat,
                remaining_amount_sat: remaining,
                fiat_amount_minor: inv.fiat_amount_minor,
                fiat_currency: inv.fiat_currency,
                memo: inv.memo,
                terminal_id: inv.terminal_id,
                accept_btc: inv.accept_btc,
                accept_ln: inv.accept_ln,
                accept_liquid: inv.accept_liquid,
                bitcoin_address: inv.bitcoin_address,
                liquid_address: inv.liquid_address,
                created_at_unix: inv.created_at_unix,
                expires_at_unix: inv.expires_at_unix,
                paid_via: inv.paid_via,
                paid_at_unix: inv.paid_at_unix,
                paid_amount_sat: inv.paid_amount_sat,
            }
        })
        .collect();
    Ok(Json(ListInvoicesResponse {
        invoices,
        page,
        page_size,
        has_more,
    }))
}

#[derive(Serialize)]
pub struct CancelInvoiceResponse {
    pub invoice_id: Uuid,
    pub status: String,
}

pub async fn cancel_invoice(
    State(state): State<AppState>,
    Path((nym, id)): Path<(String, Uuid)>,
    headers: HeaderMap,
) -> Result<Json<CancelInvoiceResponse>, AppError> {
    authenticate_terminal(&state, &nym, &headers).await?;
    let id_str = id.to_string();
    let inv = db::get_invoice_by_id(&state.db, id)
        .await?
        .ok_or_else(|| AppError::InvoiceNotFound(id_str.clone()))?;
    if inv.nym_owner.as_deref() != Some(nym.as_str()) || inv.terminal_id.is_none() {
        return Err(AppError::InvoiceNotFound(id_str));
    }
    let (_rows, final_status) = db::cancel_invoice(&state.db, id).await?;
    if matches!(
        final_status.as_str(),
        "in_progress" | "partially_paid" | "paid" | "underpaid" | "overpaid"
    ) {
        return Err(AppError::InvoicePaymentAlreadyDetected);
    }
    Ok(Json(CancelInvoiceResponse {
        invoice_id: id,
        status: final_status,
    }))
}

#[derive(Deserialize)]
pub struct ListTerminalsQuery {
    pub npub: String,
    pub timestamp: u64,
    pub signature: String,
}

#[derive(Serialize)]
pub struct TerminalItem {
    pub id: Uuid,
    pub nym: String,
    pub label: Option<String>,
    pub claimed_at_unix: Option<i64>,
    pub last_seen_at_unix: Option<i64>,
    pub revoked_at_unix: Option<i64>,
    pub created_at_unix: i64,
}

#[derive(Serialize)]
pub struct ListTerminalsResponse {
    pub terminals: Vec<TerminalItem>,
}

pub async fn list_terminals(
    State(state): State<AppState>,
    Query(params): Query<ListTerminalsQuery>,
) -> Result<Json<ListTerminalsResponse>, AppError> {
    auth::verify_la_v2(
        ACTION_TERMINAL_LIST,
        &params.npub,
        "",
        &[],
        params.timestamp,
        &params.signature,
    )?;
    let terminals = db::list_terminals_by_npub(&state.db, &params.npub)
        .await?
        .into_iter()
        .map(terminal_item)
        .collect();
    Ok(Json(ListTerminalsResponse { terminals }))
}

#[derive(Deserialize)]
pub struct RevokeTerminalRequest {
    pub npub: String,
    pub timestamp: u64,
    pub signature: String,
}

#[derive(Serialize)]
pub struct RevokeTerminalResponse {
    pub terminal_id: Uuid,
    pub revoked: bool,
}

pub async fn revoke_terminal(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<RevokeTerminalRequest>,
) -> Result<Json<RevokeTerminalResponse>, AppError> {
    let terminal_id = id.to_string();
    auth::verify_la_v2(
        ACTION_TERMINAL_REVOKE,
        &req.npub,
        "",
        &[&terminal_id],
        req.timestamp,
        &req.signature,
    )?;
    let rows = db::revoke_terminal(&state.db, id, &req.npub).await?;
    if rows == 0 {
        return Err(AppError::TerminalNotFound(terminal_id));
    }
    Ok(Json(RevokeTerminalResponse {
        terminal_id: id,
        revoked: true,
    }))
}

enum PosRateBucket {
    Create,
    Poll,
}

async fn check_ip_rate_limit(
    state: &AppState,
    peer_opt: Option<ConnectInfo<SocketAddr>>,
    headers: &HeaderMap,
    bucket: PosRateBucket,
) -> Result<(), AppError> {
    let peer = peer_opt.map(|ConnectInfo(addr)| addr);
    let ip = ip_whitelist::caller_ip(peer, headers, state.config.rate_limit.trust_forwarded_for);
    let is_whitelisted = ip
        .map(|ip| state.ip_whitelist.contains(ip))
        .unwrap_or(false);
    if is_whitelisted {
        return Ok(());
    }
    if let Some(ip) = ip {
        match bucket {
            PosRateBucket::Create => {
                state
                    .rate_limiter
                    .check_pos_pairing_create_per_source(ip)
                    .await?
            }
            PosRateBucket::Poll => {
                state
                    .rate_limiter
                    .check_pos_pairing_poll_per_source(ip)
                    .await?
            }
        }
    }
    Ok(())
}

fn terminal_item(row: db::PosTerminal) -> TerminalItem {
    TerminalItem {
        id: row.id,
        nym: row.nym,
        label: row.label,
        claimed_at_unix: row.claimed_at_unix,
        last_seen_at_unix: row.last_seen_at_unix,
        revoked_at_unix: row.revoked_at_unix,
        created_at_unix: row.created_at_unix,
    }
}

fn validate_list_query(
    page: i64,
    page_size: i64,
    status: Option<&str>,
) -> Result<(i64, i64, Option<&str>), AppError> {
    if page < 1 {
        return Err(AppError::InvalidAmount("page must be >= 1".into()));
    }
    if page > 1000 {
        return Err(AppError::InvalidAmount("page must be <= 1000".into()));
    }
    if page_size < 1 {
        return Err(AppError::InvalidAmount("pageSize must be >= 1".into()));
    }
    let page_size = page_size.min(100);
    let status_filter = match status {
        None | Some("") => None,
        Some(s)
            if matches!(
                s,
                "unpaid"
                    | "in_progress"
                    | "partially_paid"
                    | "paid"
                    | "underpaid"
                    | "overpaid"
                    | "expired"
                    | "cancelled"
            ) =>
        {
            Some(s)
        }
        Some(other) => {
            return Err(AppError::InvalidAmount(format!(
                "status must be one of unpaid|in_progress|partially_paid|paid|underpaid|overpaid|expired|cancelled, or empty (got '{other}')"
            )));
        }
    };
    Ok((page, page_size, status_filter))
}

fn pairing_not_found(nym: &str) -> AppError {
    AppError::DonationPageNotFound(nym.to_string())
}

pub(crate) fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn validate_label(label: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(label) = label else {
        return Ok(None);
    };
    if label.chars().any(|c| c.is_control()) {
        return Err(AppError::InvalidAmount(
            "label must not contain control characters".into(),
        ));
    }
    if label.chars().count() > 100 {
        return Err(AppError::InvalidAmount(
            "label too long (max 100 chars)".into(),
        ));
    }
    if label.is_empty() {
        Ok(None)
    } else {
        Ok(Some(label.to_string()))
    }
}

fn generate_pairing_code() -> String {
    let mut bytes = [0u8; PAIRING_CODE_LEN];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| PAIRING_CODE_ALPHABET[(b & 31) as usize] as char)
        .collect()
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
