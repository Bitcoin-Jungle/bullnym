use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::{get, post, put};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

use pay_service::boltz::BoltzService;
use pay_service::config::{
    BitcoinWatcherConfig, BoltzConfig, CertificationConfig, ClaimConfig, Config, DonationConfig,
    ElectrumConfig, FeaturesConfig, InvoiceAccountingConfig, LimitsConfig, PricerConfig,
    ProofConfig, PwaConfig, RateLimitConfig, ReconcilerConfig, WorkersConfig,
};
use pay_service::donation_render::PwaShells;
use pay_service::ip_whitelist::IpWhitelist;
use pay_service::pricer::PricerClient;
use pay_service::rate_limit::RateLimiter;
use pay_service::{
    certification, claimer, donation_page, donation_render, invoice, lnurl, nostr, pos,
    registration, AppState,
};

use boltz_client::network::Network;
use boltz_client::util::secrets::SwapMasterKey;
use secp256k1::{Keypair, Message, Secp256k1};
use sha2::{Digest, Sha256};

// --- Test infrastructure ---

fn require_test_db() -> String {
    std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must be set to run integration tests")
}

async fn test_pool() -> PgPool {
    let url = require_test_db();
    PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("failed to connect to test database")
}

fn test_shell_root() -> std::path::PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "bullnym-integration-pwa-{unique}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(root.join("apps").join("donation")).unwrap();
    std::fs::create_dir_all(root.join("apps").join("pos")).unwrap();
    let shell = r#"<!doctype html><head><!-- BULLNYM_OG --><!-- BULLNYM_MANIFEST --></head><body><!-- BULLNYM_CONFIG --></body>"#;
    std::fs::write(root.join("apps").join("donation").join("index.html"), shell).unwrap();
    std::fs::write(root.join("apps").join("pos").join("index.html"), shell).unwrap();
    root
}

fn test_config() -> Config {
    Config {
        domain: "test.example.com".to_string(),
        listen: "127.0.0.1:0".to_string(),
        pool_size: 2,
        boltz: BoltzConfig {
            api_url: "http://127.0.0.1:1".to_string(),
            electrum_url: "blockstream.info:995".to_string(),
        },
        pricer: PricerConfig::default(),
        pwa: PwaConfig::default(),
        donation: DonationConfig::default(),
        limits: LimitsConfig::default(),
        proof: ProofConfig::default(),
        features: FeaturesConfig::default(),
        rate_limit: RateLimitConfig::default(),
        certification: CertificationConfig::default(),
        electrum: ElectrumConfig::default(),
        claim: ClaimConfig::default(),
        reconciler: ReconcilerConfig::default(),
        bitcoin_watcher: BitcoinWatcherConfig::default(),
        workers: WorkersConfig::default(),
        invoice_accounting: InvoiceAccountingConfig::default(),
        database_url: String::new(),
        swap_mnemonic: String::new(),
        boltz_webhook_url_secret: String::new(),
        boltz_webhook_url_secret_previous: String::new(),
    }
}

fn test_state(pool: PgPool) -> AppState {
    test_state_with_config(pool, test_config())
}

fn test_state_with_config(pool: PgPool, config: Config) -> AppState {
    let swap_master_key = SwapMasterKey::from_mnemonic(
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        None,
        Network::Mainnet,
    ).unwrap();

    let rate_limiter = Arc::new(RateLimiter::new(pool.clone(), config.rate_limit.clone()));
    let pricer = Arc::new(PricerClient::new(PricerConfig::default()).unwrap());

    AppState {
        db: pool,
        config: Arc::new(config),
        boltz: Arc::new(BoltzService::new(
            "http://127.0.0.1:1",
            swap_master_key,
            None,
        )),
        ip_whitelist: Arc::new(IpWhitelist::default()),
        certification: Arc::new(certification::CertificationAllowlist::default()),
        rate_limiter,
        utxo_backend: None,
        pricer,
        pwa_shells: Arc::new(PwaShells::default()),
    }
}

fn test_app(state: AppState) -> Router {
    Router::new()
        .route("/.well-known/lnurlp/:nym", get(lnurl::metadata))
        .route("/.well-known/nostr.json", get(nostr::nostr_json))
        .route("/lnurlp/callback/:nym", get(lnurl::callback))
        .route("/donation-page", put(donation_page::save))
        .route("/sw.js", get(donation_render::service_worker))
        .route("/:nym/manifest.webmanifest", get(donation_render::manifest))
        .route("/:nym/pos", get(donation_render::render_pos_or_404))
        .route(
            "/:nym/pos/manifest.webmanifest",
            get(donation_render::pos_manifest),
        )
        .route("/register", post(registration::register))
        .route("/register", put(registration::update_registration))
        .route(
            "/register",
            axum::routing::delete(registration::delete_registration),
        )
        .route("/register/lookup", get(registration::lookup_by_npub))
        .route("/api/v1/:nym/invoices", post(invoice::create_signed_linked))
        .route("/api/v1/invoices", post(invoice::create_signed_unlinked))
        .route("/api/v1/invoices", get(invoice::list_signed))
        .route("/:nym/invoice", post(invoice::create_anonymous))
        .route(
            "/api/v1/:nym/invoices/:id",
            axum::routing::delete(invoice::cancel_linked),
        )
        .route(
            "/api/v1/invoices/:id",
            axum::routing::delete(invoice::cancel_unlinked),
        )
        .route("/:nym/i/:id", get(invoice::render_payment))
        .route("/invoice/:id", get(invoice::render_unlinked_payment))
        .route("/api/v1/invoices/:id/status", get(invoice::status))
        .route("/:nym/pos/pairings", post(pos::create_pairing))
        .route("/:nym/pos/pairings/:id", get(pos::poll_pairing))
        .route("/:nym/pos/invoice", post(pos::create_invoice))
        .route("/:nym/pos/invoices", get(pos::list_invoices))
        .route("/:nym/pos/invoices/:id/cancel", post(pos::cancel_invoice))
        .route("/api/v1/pos/pairings/claim", post(pos::claim_pairing))
        .route("/api/v1/pos/terminals", get(pos::list_terminals))
        .route(
            "/api/v1/pos/terminals/:id/revoke",
            post(pos::revoke_terminal),
        )
        .route("/certification/preflight", get(certification::preflight))
        .route("/webhook/boltz", post(claimer::webhook_unauthenticated))
        .fallback(donation_render::render_or_404)
        .with_state(state)
}

fn auth_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn sign_registration(nym: &str, ct_descriptor: &str) -> (String, String, u64) {
    let (npub, sig, timestamp, _) = sign_registration_with_keypair(nym, ct_descriptor);
    (npub, sig, timestamp)
}

fn sign_registration_with_keypair(
    nym: &str,
    ct_descriptor: &str,
) -> (String, String, u64, Keypair) {
    let secp = Secp256k1::new();
    let keypair = Keypair::new(&secp, &mut secp256k1::rand::thread_rng());
    let (xonly, _) = keypair.x_only_public_key();
    let npub_hex = xonly.to_string();
    let (sig, timestamp) = sign_register_with_keypair(&keypair, &npub_hex, nym, ct_descriptor);
    (npub_hex, sig, timestamp, keypair)
}

fn sign_with_keypair(keypair: &Keypair, message: &[u8]) -> String {
    let secp = Secp256k1::new();
    let digest = Sha256::digest(message);
    let msg = Message::from_digest(*digest.as_ref());
    secp.sign_schnorr(&msg, keypair).to_string()
}

fn sign_la_action_with_timestamp(
    keypair: &Keypair,
    action: &str,
    npub: &str,
    nym: &str,
    payload_fields: &[&str],
    timestamp: u64,
) -> String {
    let message =
        pay_service::auth::build_la_v2_message(action, npub, nym, payload_fields, timestamp);
    sign_with_keypair(keypair, &message)
}

fn sign_la_action(
    keypair: &Keypair,
    action: &str,
    npub: &str,
    nym: &str,
    payload_fields: &[&str],
) -> (String, u64) {
    let timestamp = auth_timestamp();
    let sig = sign_la_action_with_timestamp(keypair, action, npub, nym, payload_fields, timestamp);
    (sig, timestamp)
}

fn sign_register_with_keypair(
    keypair: &Keypair,
    npub: &str,
    nym: &str,
    ct_descriptor: &str,
) -> (String, u64) {
    sign_register_with_verification_keypair(keypair, npub, nym, ct_descriptor, npub)
}

fn sign_register_with_verification_keypair(
    keypair: &Keypair,
    npub: &str,
    nym: &str,
    ct_descriptor: &str,
    verification_npub: &str,
) -> (String, u64) {
    sign_la_action(
        keypair,
        "register",
        npub,
        nym,
        &[ct_descriptor, verification_npub],
    )
}

fn sign_delete_with_keypair(keypair: &Keypair, npub: &str, nym: &str) -> (String, u64) {
    sign_la_action(keypair, "delete", npub, nym, &[])
}

fn sign_purge_with_keypair(keypair: &Keypair, npub: &str, nym: &str) -> (String, u64) {
    sign_la_action(keypair, "purge", npub, nym, &[])
}

fn sign_invoice_create_with_keypair(
    keypair: &Keypair,
    npub: &str,
    bitcoin_address: &str,
    expires_at_unix: i64,
) -> (String, u64) {
    let amount_sat = "1000";
    let fiat_amount_minor = "";
    let fiat_currency = "";
    let public_description = "";
    let recipient_name = "";
    let invoice_number = "";
    let accept_btc = "true";
    let accept_ln = "false";
    let accept_liquid = "false";
    let liquid_address = "";
    let liquid_blinding_key_hex = "";
    let expires_at = expires_at_unix.to_string();
    sign_la_action(
        keypair,
        "invoice-create",
        npub,
        "",
        &[
            amount_sat,
            fiat_amount_minor,
            fiat_currency,
            public_description,
            recipient_name,
            invoice_number,
            accept_btc,
            accept_ln,
            accept_liquid,
            bitcoin_address,
            liquid_address,
            liquid_blinding_key_hex,
            &expires_at,
        ],
    )
}

fn sign_invoice_create_without_expiry_with_keypair(
    keypair: &Keypair,
    npub: &str,
    bitcoin_address: &str,
) -> (String, u64) {
    let amount_sat = "1000";
    let fiat_amount_minor = "";
    let fiat_currency = "";
    let public_description = "";
    let recipient_name = "";
    let invoice_number = "";
    let accept_btc = "true";
    let accept_ln = "false";
    let accept_liquid = "false";
    let liquid_address = "";
    let liquid_blinding_key_hex = "";
    let expires_at = "";
    sign_la_action(
        keypair,
        "invoice-create",
        npub,
        "",
        &[
            amount_sat,
            fiat_amount_minor,
            fiat_currency,
            public_description,
            recipient_name,
            invoice_number,
            accept_btc,
            accept_ln,
            accept_liquid,
            bitcoin_address,
            liquid_address,
            liquid_blinding_key_hex,
            expires_at,
        ],
    )
}

fn sign_invoice_cancel_with_keypair(
    keypair: &Keypair,
    npub: &str,
    nym: &str,
    invoice_id: &str,
) -> (String, u64) {
    sign_la_action(keypair, "invoice-cancel", npub, nym, &[invoice_id])
}

fn sign_invoice_list_with_keypair(
    keypair: &Keypair,
    npub: &str,
    page: i64,
    page_size: i64,
    status: &str,
) -> (String, u64) {
    let page = page.to_string();
    let page_size = page_size.to_string();
    sign_la_action(
        keypair,
        "invoice-list",
        npub,
        "",
        &[&page, &page_size, status],
    )
}

fn sign_pos_pair_with_keypair(
    keypair: &Keypair,
    npub: &str,
    nym: &str,
    code: &str,
    label: &str,
) -> (String, u64) {
    sign_la_action(
        keypair,
        "pos-pair",
        npub,
        nym,
        &[code, label, TEST_POS_DESCRIPTOR],
    )
}

fn sign_pos_terminal_list_with_keypair(keypair: &Keypair, npub: &str) -> (String, u64) {
    sign_la_action(keypair, "pos-terminal-list", npub, "", &[])
}

fn sign_pos_terminal_revoke_with_keypair(
    keypair: &Keypair,
    npub: &str,
    terminal_id: &str,
) -> (String, u64) {
    sign_la_action(keypair, "pos-terminal-revoke", npub, "", &[terminal_id])
}

struct DonationSaveSignFields<'a> {
    header: &'a str,
    description: &'a str,
    display_currency: &'a str,
    website: &'a str,
    twitter: &'a str,
    instagram: &'a str,
    enabled: bool,
    ct_descriptor: Option<&'a str>,
}

fn sign_donation_page_save_with_keypair(
    keypair: &Keypair,
    npub: &str,
    nym: &str,
    save: DonationSaveSignFields<'_>,
) -> (String, u64) {
    let enabled_str = if save.enabled { "1" } else { "0" };
    let mut fields = vec![
        save.header,
        save.description,
        save.display_currency,
        save.website,
        save.twitter,
        save.instagram,
        enabled_str,
    ];
    if let Some(ct_descriptor) = save.ct_descriptor {
        fields.push(ct_descriptor);
    }
    sign_la_action(keypair, "donation-page-save", npub, nym, &fields)
}

// Valid CT descriptor (lwk 0.14, h-notation)
const TEST_DESCRIPTOR: &str = "ct(slip77(9c8e4f05c7711a98c838be228bcb84924d4570ca53f35fa1c793e58841d47023),elwpkh([73c5da0a/84h/1776h/0h]xpub6CRFzUgHFDaiDAQFNX7VeV9JNPDRabq6NYSpzVZ8zW8ANUCiDdenkb1gBoEZuXNZb3wPc1SVcDXgD2ww5UBtTb8s8ArAbTkoRQ8qn34KgcY/<0;1>/*))#y8jljyxl";
const TEST_POS_DESCRIPTOR: &str = TEST_DESCRIPTOR;
const OTHER_TEST_POS_DESCRIPTOR: &str = "ct(slip77(8c8e4f05c7711a98c838be228bcb84924d4570ca53f35fa1c793e58841d47023),elwpkh([73c5da0a/84h/1776h/0h]xpub6CRFzUgHFDaiDAQFNX7VeV9JNPDRabq6NYSpzVZ8zW8ANUCiDdenkb1gBoEZuXNZb3wPc1SVcDXgD2ww5UBtTb8s8ArAbTkoRQ8qn34KgcY/<0;1>/*))";

async fn cleanup_db(pool: &PgPool) {
    sqlx::query("DELETE FROM processed_webhook_events")
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM chain_swap_records")
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM swap_records")
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM invoices").execute(pool).await.ok();
    sqlx::query("DELETE FROM pos_terminals")
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM donation_pages")
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM users").execute(pool).await.ok();
}

async fn post_json(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn post_json_with_headers(
    app: &Router,
    uri: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let resp = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn get_path(app: &Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn get_path_with_headers(
    app: &Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let resp = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn get_json_with_headers(app: &Router, uri: &str) -> (StatusCode, HeaderMap, Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, body)
}

async fn get_text_with_headers(app: &Router, uri: &str) -> (StatusCode, HeaderMap, String) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&bytes).to_string();
    (status, headers, body)
}

async fn put_json(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn delete_json_path(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

// --- Registration tests ---

#[tokio::test]
async fn donation_page_upsert_round_trips_enabled() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    create_test_user(&pool, "posround").await;

    let row = pay_service::db::upsert_donation_page(
        &pool,
        &pay_service::db::UpsertDonationPage {
            nym: "posround",
            ct_descriptor: Some(TEST_DESCRIPTOR),
            header: "POS Store",
            description: "Counter checkout",
            display_currency: "USD",
            website: None,
            twitter: None,
            instagram: None,
            enabled: true,
        },
    )
    .await
    .unwrap();
    assert!(row.enabled);

    let fetched = pay_service::db::get_donation_page_by_nym(&pool, "posround")
        .await
        .unwrap()
        .unwrap();
    assert!(fetched.enabled);

    let row = pay_service::db::upsert_donation_page(
        &pool,
        &pay_service::db::UpsertDonationPage {
            nym: "posround",
            ct_descriptor: None,
            header: "Donation Store",
            description: "Tip jar",
            display_currency: "CAD",
            website: Some("https://example.com"),
            twitter: Some("posround"),
            instagram: None,
            enabled: false,
        },
    )
    .await
    .unwrap();
    assert!(!row.enabled);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn manifest_falls_back_to_nym_and_sets_pwa_metadata() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let nym = "manifestnym";
    create_test_user(&pool, nym).await;

    pay_service::db::upsert_donation_page(
        &pool,
        &pay_service::db::UpsertDonationPage {
            nym,
            ct_descriptor: Some(TEST_DESCRIPTOR),
            header: "",
            description: "Manifest test",
            display_currency: "USD",
            website: None,
            twitter: None,
            instagram: None,
            enabled: true,
        },
    )
    .await
    .unwrap();

    let (status, headers, body) =
        get_json_with_headers(&app, "/manifestnym/manifest.webmanifest").await;

    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/manifest+json")
    );
    assert_eq!(
        headers
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("public, max-age=300")
    );
    assert_eq!(body["name"], "manifestnym");
    assert_eq!(body["short_name"], "manifestnym");
    assert_eq!(body["start_url"], "/manifestnym");
    assert_eq!(body["scope"], "/");
    assert_eq!(body["display"], "standalone");
    assert_eq!(body["background_color"], "#161512");
    assert_eq!(body["theme_color"], "#161512");
    assert_eq!(body["icons"].as_array().expect("icons array").len(), 4);
    assert_eq!(body["icons"][0]["src"], "/pwa-assets/icons/icon-192.png");
    assert_eq!(body["icons"][0]["sizes"], "192x192");
    assert_eq!(body["icons"][0]["type"], "image/png");
    assert_eq!(body["icons"][0]["purpose"], "any");
    assert_eq!(body["icons"][1]["src"], "/pwa-assets/icons/icon-192.png");
    assert_eq!(body["icons"][1]["purpose"], "maskable");
    assert_eq!(body["icons"][2]["src"], "/pwa-assets/icons/icon-512.png");
    assert_eq!(body["icons"][2]["purpose"], "any");
    assert_eq!(body["icons"][3]["src"], "/pwa-assets/icons/icon-512.png");
    assert_eq!(body["icons"][3]["purpose"], "maskable");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn manifest_returns_404_for_unknown_nym() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (status, _, body) = get_json_with_headers(&app, "/unknownnym/manifest.webmanifest").await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_render_serves_pos_shell_for_enabled_and_disabled_pages() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let shell_root = test_shell_root();
    let mut state = test_state(pool.clone());
    state.pwa_shells = Arc::new(PwaShells::load(&shell_root));
    let app = test_app(state);

    create_test_user(&pool, "posenabled").await;
    create_test_pos_page(&pool, "posenabled", true).await;
    create_test_user(&pool, "posdisabled").await;
    create_test_pos_page(&pool, "posdisabled", false).await;
    create_test_user(&pool, "posarchived").await;
    create_test_pos_page(&pool, "posarchived", true).await;
    sqlx::query("UPDATE donation_pages SET archived_at = NOW() WHERE nym = $1")
        .bind("posarchived")
        .execute(&pool)
        .await
        .unwrap();

    for nym in ["posenabled", "posdisabled"] {
        let (status, headers, body) = get_text_with_headers(&app, &format!("/{nym}/pos")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            headers
                .get("x-bullnym-pwa-shell")
                .and_then(|value| value.to_str().ok()),
            Some("pos")
        );
        assert_eq!(
            headers
                .get("content-security-policy")
                .and_then(|value| value.to_str().ok()),
            Some("default-src 'self'; img-src 'self' data:; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self' https: wss://liquid.network wss://liquid.bullbitcoin.com; frame-ancestors 'none'; base-uri 'none'")
        );
        assert!(body.contains(r#""mode":"pos""#));
        assert!(body.contains(&format!(
            r#"<link rel="manifest" href="/{nym}/pos/manifest.webmanifest">"#
        )));
    }

    let (archived_status, _, _) = get_text_with_headers(&app, "/posarchived/pos").await;
    assert_eq!(archived_status, StatusCode::NOT_FOUND);
    let (missing_status, _, _) = get_text_with_headers(&app, "/posmissing/pos").await;
    assert_eq!(missing_status, StatusCode::NOT_FOUND);

    std::fs::remove_dir_all(shell_root).ok();
    cleanup_db(&pool).await;
}

#[tokio::test]
async fn donation_render_stays_donation_shell_for_enabled_pages_with_terminals() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let shell_root = test_shell_root();
    let mut state = test_state(pool.clone());
    state.pwa_shells = Arc::new(PwaShells::load(&shell_root));
    let app = test_app(state);

    let npub = create_test_user(&pool, "donationpos").await;
    create_test_pos_page(&pool, "donationpos", true).await;
    create_claimed_terminal(&pool, "donationpos", &npub, "donationpos-token").await;

    let (status, headers, body) = get_text_with_headers(&app, "/donationpos").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        headers
            .get("x-bullnym-pwa-shell")
            .and_then(|value| value.to_str().ok()),
        Some("donation")
    );
    assert_eq!(
        headers
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok()),
        Some("default-src 'self'; img-src 'self' data:; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self' wss://liquid.network wss://liquid.bullbitcoin.com; frame-ancestors 'none'; base-uri 'none'")
    );
    assert!(body.contains(r#""mode":"donation""#));
    assert!(body.contains(r#"<link rel="manifest" href="/donationpos/manifest.webmanifest">"#));

    std::fs::remove_dir_all(shell_root).ok();
    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_manifest_uses_pos_start_url_and_name_without_changing_donation_manifest() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let nym = "manifestpos";
    create_test_user(&pool, nym).await;
    create_test_pos_page(&pool, nym, false).await;

    let (donation_status, _, donation_body) =
        get_json_with_headers(&app, "/manifestpos/manifest.webmanifest").await;
    assert_eq!(donation_status, StatusCode::NOT_FOUND, "{donation_body:?}");

    sqlx::query("UPDATE donation_pages SET enabled = TRUE WHERE nym = $1")
        .bind(nym)
        .execute(&pool)
        .await
        .unwrap();
    let (donation_status, _, donation_body) =
        get_json_with_headers(&app, "/manifestpos/manifest.webmanifest").await;
    assert_eq!(donation_status, StatusCode::OK, "{donation_body:?}");
    assert_eq!(donation_body["name"], "POS");
    assert_eq!(donation_body["start_url"], "/manifestpos");

    sqlx::query("UPDATE donation_pages SET enabled = FALSE WHERE nym = $1")
        .bind(nym)
        .execute(&pool)
        .await
        .unwrap();
    let (pos_status, _, pos_body) =
        get_json_with_headers(&app, "/manifestpos/pos/manifest.webmanifest").await;
    assert_eq!(pos_status, StatusCode::OK, "{pos_body:?}");
    assert_eq!(pos_body["name"], "POS POS");
    assert_eq!(pos_body["start_url"], "/manifestpos/pos");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn donation_page_save_uses_legacy_payload_without_pos_mode() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let nym = "poslegacy";
    let (npub, _, _, keypair) = sign_registration_with_keypair(nym, TEST_DESCRIPTOR);
    pay_service::db::create_user(&pool, nym, &npub, TEST_DESCRIPTOR)
        .await
        .unwrap();
    let (signature, timestamp) = sign_donation_page_save_with_keypair(
        &keypair,
        &npub,
        nym,
        DonationSaveSignFields {
            header: "Legacy Save",
            description: "Clients sign the stable field list",
            display_currency: "USD",
            website: "",
            twitter: "",
            instagram: "",
            enabled: true,
            ct_descriptor: Some(TEST_DESCRIPTOR),
        },
    );
    let (status, body) = put_json(
        &app,
        "/donation-page",
        json!({
            "nym": nym,
            "npub": npub,
            "ct_descriptor": TEST_DESCRIPTOR,
            "header": "Legacy Save",
            "description": "Clients sign the stable field list",
            "display_currency": "USD",
            "enabled": true,
            "timestamp": timestamp,
            "signature": signature,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert!(body.get("pos_mode").is_none());

    let row = pay_service::db::get_donation_page_by_nym(&pool, nym)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.header, "Legacy Save");
    assert!(row.enabled);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn register_and_resolve() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (npub, sig, timestamp) = sign_registration("alice", TEST_DESCRIPTOR);
    let (status, body) = post_json(
        &app,
        "/register",
        json!({
            "nym": "alice",
            "ct_descriptor": TEST_DESCRIPTOR,
            "npub": npub,
            "verification_npub": npub,
            "signature": sig,
            "timestamp": timestamp,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["nym"], "alice");
    assert_eq!(body["lightning_address"], "alice@test.example.com");
    assert_eq!(body["nip05"], "alice@test.example.com");

    // LNURL metadata resolves
    let (status, body) = get_path(&app, "/.well-known/lnurlp/alice").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tag"], "payRequest");
    assert!(body["callback"].as_str().unwrap().contains("alice"));

    // NIP-05 resolves
    let (status, body) = get_path(&app, "/.well-known/nostr.json?name=alice").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["names"]["alice"], npub);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn lookup_returns_lightning_address_only_while_active() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (npub, sig, timestamp, keypair) =
        sign_registration_with_keypair("lookupnym", TEST_DESCRIPTOR);
    let (status, _) = post_json(
        &app,
        "/register",
        json!({
            "nym": "lookupnym",
            "ct_descriptor": TEST_DESCRIPTOR,
            "npub": npub,
            "verification_npub": npub,
            "signature": sig,
            "timestamp": timestamp,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = get_path(&app, &format!("/register/lookup?npub={npub}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["nym"], "lookupnym");
    assert_eq!(body["active"], true);
    assert_eq!(body["lightning_address"], "lookupnym@test.example.com");

    let (sig, timestamp) = sign_delete_with_keypair(&keypair, &npub, "lookupnym");
    let (status, _) = delete_json_path(
        &app,
        "/register",
        json!({
            "nym": "lookupnym",
            "npub": npub,
            "signature": sig,
            "timestamp": timestamp,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get_path(&app, &format!("/register/lookup?npub={npub}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["active"], false);
    assert_eq!(body["lightning_address"], Value::Null);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn legacy_register_without_verification_npub_still_resolves() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let secp = Secp256k1::new();
    let keypair = Keypair::new(&secp, &mut secp256k1::rand::thread_rng());
    let (xonly, _) = keypair.x_only_public_key();
    let npub = xonly.to_string();
    let (sig, timestamp) =
        sign_la_action(&keypair, "register", &npub, "legacyreg", &[TEST_DESCRIPTOR]);

    let (status, _) = post_json(
        &app,
        "/register",
        json!({
            "nym": "legacyreg",
            "ct_descriptor": TEST_DESCRIPTOR,
            "npub": npub,
            "signature": sig,
            "timestamp": timestamp,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = get_path(&app, "/.well-known/nostr.json?name=legacyreg").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["names"]["legacyreg"], npub);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn register_nip05_resolves_verification_npub() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let secp = Secp256k1::new();
    let auth_keypair = Keypair::new(&secp, &mut secp256k1::rand::thread_rng());
    let (auth_xonly, _) = auth_keypair.x_only_public_key();
    let auth_npub = auth_xonly.to_string();
    let verification_keypair = Keypair::new(&secp, &mut secp256k1::rand::thread_rng());
    let (verification_xonly, _) = verification_keypair.x_only_public_key();
    let verification_npub = verification_xonly.to_string();
    let (sig, timestamp) = sign_register_with_verification_keypair(
        &auth_keypair,
        &auth_npub,
        "verifykey",
        TEST_DESCRIPTOR,
        &verification_npub,
    );

    let (status, _) = post_json(
        &app,
        "/register",
        json!({
            "nym": "verifykey",
            "ct_descriptor": TEST_DESCRIPTOR,
            "npub": auth_npub,
            "verification_npub": verification_npub,
            "signature": sig,
            "timestamp": timestamp,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = get_path(&app, "/.well-known/nostr.json?name=verifykey").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["names"]["verifykey"], verification_npub);
    assert_ne!(body["names"]["verifykey"], auth_npub);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn register_duplicate_nym_rejected() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (npub1, sig1, timestamp1) = sign_registration("taken", TEST_DESCRIPTOR);
    post_json(
        &app,
        "/register",
        json!({
            "nym": "taken", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub1, "verification_npub": npub1, "signature": sig1, "timestamp": timestamp1,
        }),
    )
    .await;

    let (npub2, sig2, timestamp2) = sign_registration("taken", TEST_DESCRIPTOR);
    let (status, body) = post_json(
        &app,
        "/register",
        json!({
            "nym": "taken", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub2, "verification_npub": npub2, "signature": sig2, "timestamp": timestamp2,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ERROR");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn register_bad_signature_rejected() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (npub, _, timestamp) = sign_registration("badsig", TEST_DESCRIPTOR);
    let (status, _) = post_json(&app, "/register", json!({
        "nym": "badsig", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": "aa".repeat(32), "timestamp": timestamp,
    })).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    cleanup_db(&pool).await;
}

#[tokio::test]
async fn register_invalid_nym_rejected() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    // "a" removed: one-character nyms are valid since ef7e11b.
    for bad_nym in ["AB", "-bad", "bad-", "has space", "has_under", "a@b"] {
        let (npub, sig, timestamp) = sign_registration(bad_nym, TEST_DESCRIPTOR);
        let (_, body) = post_json(
            &app,
            "/register",
            json!({
                "nym": bad_nym, "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": sig, "timestamp": timestamp,
            }),
        )
        .await;
        assert_eq!(
            body["status"], "ERROR",
            "nym '{bad_nym}' should be rejected"
        );
    }

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn unknown_nym_returns_lnurl_error() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (status, body) = get_path(&app, "/.well-known/lnurlp/nobody").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ERROR");

    cleanup_db(&pool).await;
}

// --- Address index allocation ---

#[tokio::test]
async fn address_indices_are_sequential() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;

    let (npub, _, _) = sign_registration("idxuser", TEST_DESCRIPTOR);
    pay_service::db::create_user(&pool, "idxuser", &npub, TEST_DESCRIPTOR)
        .await
        .unwrap();

    for expected in 0..5 {
        let idx = pay_service::db::allocate_address_index(&pool, "idxuser")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idx, expected);
    }

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn concurrent_address_allocation_no_duplicates() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;

    let (npub, _, _) = sign_registration("concuser", TEST_DESCRIPTOR);
    pay_service::db::create_user(&pool, "concuser", &npub, TEST_DESCRIPTOR)
        .await
        .unwrap();

    let mut handles = Vec::new();
    for _ in 0..10 {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            pay_service::db::allocate_address_index(&pool, "concuser")
                .await
                .unwrap()
                .unwrap()
        }));
    }

    let mut indices: Vec<i32> = Vec::new();
    for h in handles {
        indices.push(h.await.unwrap());
    }
    indices.sort();

    let unique: std::collections::HashSet<i32> = indices.iter().cloned().collect();
    assert_eq!(unique.len(), 10, "all 10 indices must be unique");
    assert_eq!(*indices.first().unwrap(), 0);
    assert_eq!(*indices.last().unwrap(), 9);

    cleanup_db(&pool).await;
}

// --- Webhook parsing ---

#[tokio::test]
async fn webhook_parses_boltz_envelope() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    // Webhook for unknown swap is acknowledged so Boltz does not retry a
    // swap we never created or already purged.
    let (status, body) = post_json(
        &app,
        "/webhook/boltz",
        json!({
            "event": "swap.update",
            "data": {"id": "nonexistent", "status": "transaction.mempool"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Null);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn webhook_rejects_malformed_payload() {
    let pool = test_pool().await;
    let app = test_app(test_state(pool.clone()));

    // Missing data field
    let (status, body) = post_json(&app, "/webhook/boltz", json!({"id": "x", "status": "y"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ERROR");
}

#[tokio::test]
async fn webhook_skips_terminal_swaps() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let state = test_state(pool.clone());
    let app = test_app(state);

    // Create a user and a fake swap record in "claimed" state
    cleanup_db(&pool).await;
    let (npub, _, _) = sign_registration("webhookuser", TEST_DESCRIPTOR);
    pay_service::db::create_user(&pool, "webhookuser", &npub, TEST_DESCRIPTOR)
        .await
        .unwrap();

    pay_service::db::record_swap(
        &pool,
        &pay_service::db::NewSwapRecord {
            nym: Some("webhookuser"),
            boltz_swap_id: "FAKE_CLAIMED",
            address: Some("lq1qqtest"),
            address_index: Some(0),
            amount_sat: 1000,
            invoice: "lnbc...",
            preimage_hex: "aa".repeat(32).as_str(),
            claim_key_hex: "bb".repeat(32).as_str(),
            boltz_response_json: "{}",
            invoice_id: None,
        },
    )
    .await
    .unwrap();

    // Mark as claimed
    let swap = pay_service::db::get_swap_by_boltz_id(&pool, "FAKE_CLAIMED")
        .await
        .unwrap()
        .unwrap();
    pay_service::db::update_swap_status(
        &pool,
        swap.id,
        pay_service::db::SwapStatus::Claimed,
        Some("txid123"),
    )
    .await
    .unwrap();

    // Webhook should be silently accepted (not trigger a re-claim)
    let (status, _) = post_json(
        &app,
        "/webhook/boltz",
        json!({
            "event": "swap.update",
            "data": {"id": "FAKE_CLAIMED", "status": "transaction.confirmed"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Status should still be claimed
    let swap = pay_service::db::get_swap_by_boltz_id(&pool, "FAKE_CLAIMED")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(swap.status, "claimed");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn webhook_advances_chain_swap_records() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let state = test_state(pool.clone());
    let app = test_app(state);

    let npub = create_test_user(&pool, "chainwebhook").await;
    let invoice = insert_test_invoice(&pool, "chainwebhook", &npub, "lq1chainwebhook", 60).await;
    pay_service::db::record_chain_swap(
        &pool,
        &pay_service::db::NewChainSwapRecord {
            invoice_id: invoice.id,
            nym: Some("chainwebhook"),
            boltz_swap_id: "CHAIN_WEBHOOK_1",
            lockup_address: "bc1qchainwebhooklockup",
            lockup_bip21: None,
            user_lock_amount_sat: 1_000,
            server_lock_amount_sat: 990,
            preimage_hex: "11".repeat(32).as_str(),
            claim_key_hex: "22".repeat(32).as_str(),
            refund_key_hex: "33".repeat(32).as_str(),
            boltz_response_json: "{\"id\":\"CHAIN_WEBHOOK_1\"}",
        },
    )
    .await
    .unwrap();

    let (status, _) = post_json(
        &app,
        "/webhook/boltz",
        json!({
            "event": "swap.update",
            "data": {"id": "CHAIN_WEBHOOK_1", "status": "transaction.server.confirmed"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let row = pay_service::db::get_chain_swap_by_boltz_id(&pool, "CHAIN_WEBHOOK_1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "server_lock_confirmed");
    let invoice_after = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(invoice_after.status, "in_progress");
    assert_eq!(invoice_after.settlement_status, "pending");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn webhook_skips_terminal_chain_swap_records() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let state = test_state(pool.clone());
    let app = test_app(state);

    let npub = create_test_user(&pool, "chainterminal").await;
    let invoice = insert_test_invoice(&pool, "chainterminal", &npub, "lq1chainterminal", 60).await;
    let row = pay_service::db::record_chain_swap(
        &pool,
        &pay_service::db::NewChainSwapRecord {
            invoice_id: invoice.id,
            nym: Some("chainterminal"),
            boltz_swap_id: "CHAIN_TERMINAL_1",
            lockup_address: "bc1qchainterminallockup",
            lockup_bip21: None,
            user_lock_amount_sat: 1_000,
            server_lock_amount_sat: 990,
            preimage_hex: "11".repeat(32).as_str(),
            claim_key_hex: "22".repeat(32).as_str(),
            refund_key_hex: "33".repeat(32).as_str(),
            boltz_response_json: "{\"id\":\"CHAIN_TERMINAL_1\"}",
        },
    )
    .await
    .unwrap();
    pay_service::db::update_chain_swap_status(
        &pool,
        row.id,
        pay_service::db::ChainSwapStatus::Claimed,
        Some("chain-claim-txid"),
    )
    .await
    .unwrap();

    let (status, _) = post_json(
        &app,
        "/webhook/boltz",
        json!({
            "event": "swap.update",
            "data": {"id": "CHAIN_TERMINAL_1", "status": "transaction.refunded"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let row = pay_service::db::get_chain_swap_by_boltz_id(&pool, "CHAIN_TERMINAL_1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "claimed");
    assert_eq!(row.claim_txid.as_deref(), Some("chain-claim-txid"));

    cleanup_db(&pool).await;
}

// --- LNURL callback validation ---

#[tokio::test]
async fn callback_rejects_invalid_amounts() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let state = test_state(pool.clone());
    let app = test_app(state);

    // Register a user first
    let (npub, sig, timestamp) = sign_registration("amtuser", TEST_DESCRIPTOR);
    post_json(
        &app,
        "/register",
        json!({
            "nym": "amtuser", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": sig, "timestamp": timestamp,
        }),
    )
    .await;

    // Below minimum (default 100k msat = 100 sats)
    let (_, body) = get_path(&app, "/lnurlp/callback/amtuser?amount=1000").await;
    assert_eq!(body["status"], "ERROR");

    // Not divisible by 1000
    let (_, body) = get_path(&app, "/lnurlp/callback/amtuser?amount=100500").await;
    assert_eq!(body["status"], "ERROR");

    // Above maximum
    let (_, body) = get_path(&app, "/lnurlp/callback/amtuser?amount=99000000000000").await;
    assert_eq!(body["status"], "ERROR");

    cleanup_db(&pool).await;
}

// --- Delete registration ---

#[tokio::test]
async fn delete_registration_deactivates_user() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let secp = Secp256k1::new();
    let keypair = Keypair::new(&secp, &mut secp256k1::rand::thread_rng());
    let (xonly, _) = keypair.x_only_public_key();
    let npub_hex = xonly.to_string();

    // Register
    let (sig, timestamp) =
        sign_register_with_keypair(&keypair, &npub_hex, "deluser", TEST_DESCRIPTOR);

    post_json(&app, "/register", json!({
        "nym": "deluser", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub_hex, "verification_npub": npub_hex, "signature": sig, "timestamp": timestamp,
    })).await;

    // Delete
    let (del_sig, del_timestamp) = sign_delete_with_keypair(&keypair, &npub_hex, "deluser");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"npub": npub_hex, "nym": "deluser", "signature": del_sig, "timestamp": del_timestamp}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // LNURL should no longer resolve
    let (_, body) = get_path(&app, "/.well-known/lnurlp/deluser").await;
    assert_eq!(body["status"], "ERROR");

    cleanup_db(&pool).await;
}

// --- Nym lifecycle tests ---

#[tokio::test]
async fn reregister_after_delete_succeeds() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (npub, sig, timestamp, keypair) =
        sign_registration_with_keypair("lifecycle1", TEST_DESCRIPTOR);

    // Register
    let (status, _) = post_json(
        &app,
        "/register",
        json!({
            "nym": "lifecycle1", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": sig, "timestamp": timestamp,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Delete
    let (del_sig, del_timestamp) = sign_delete_with_keypair(&keypair, &npub, "lifecycle1");
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"npub": npub, "nym": "lifecycle1", "signature": del_sig, "timestamp": del_timestamp}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Re-register with new nym, same npub
    let (new_sig, new_timestamp) =
        sign_register_with_keypair(&keypair, &npub, "lifecycle2", TEST_DESCRIPTOR);
    let (status, body) = post_json(&app, "/register", json!({
        "nym": "lifecycle2", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": new_sig, "timestamp": new_timestamp,
    })).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["nym"], "lifecycle2");

    // New nym resolves
    let (_, body) = get_path(&app, "/.well-known/lnurlp/lifecycle2").await;
    assert_eq!(body["tag"], "payRequest");

    // Old nym does not resolve
    let (_, body) = get_path(&app, "/.well-known/lnurlp/lifecycle1").await;
    assert_eq!(body["status"], "ERROR");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn reregister_same_nym_after_delete() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (npub, sig, timestamp, keypair) =
        sign_registration_with_keypair("samename", TEST_DESCRIPTOR);

    // Register
    post_json(
        &app,
        "/register",
        json!({
            "nym": "samename", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": sig, "timestamp": timestamp,
        }),
    )
    .await;

    // Delete
    let (del_sig, del_timestamp) = sign_delete_with_keypair(&keypair, &npub, "samename");
    app.clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"npub": npub, "nym": "samename", "signature": del_sig, "timestamp": del_timestamp}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    // Re-register same nym — should reactivate
    let (re_sig, re_timestamp) =
        sign_register_with_keypair(&keypair, &npub, "samename", TEST_DESCRIPTOR);
    let (status, body) = post_json(
        &app,
        "/register",
        json!({
            "nym": "samename", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": re_sig, "timestamp": re_timestamp,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["nym"], "samename");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn register_while_active_rejected() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (npub, sig, timestamp, keypair) =
        sign_registration_with_keypair("active1", TEST_DESCRIPTOR);

    // Register first nym
    let (status, _) = post_json(
        &app,
        "/register",
        json!({
            "nym": "active1", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": sig, "timestamp": timestamp,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Try registering second nym with same npub while first is active
    let (sig2, timestamp2) =
        sign_register_with_keypair(&keypair, &npub, "active2", TEST_DESCRIPTOR);
    let (status, body) = post_json(
        &app,
        "/register",
        json!({
            "nym": "active2", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": sig2, "timestamp": timestamp2,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ERROR");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn deleted_nym_reserved_from_others() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (npub1, sig1, timestamp1, keypair1) =
        sign_registration_with_keypair("reserved", TEST_DESCRIPTOR);

    // User 1 registers and deletes
    post_json(
        &app,
        "/register",
        json!({
            "nym": "reserved", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub1, "verification_npub": npub1, "signature": sig1, "timestamp": timestamp1,
        }),
    )
    .await;

    let (del_sig, del_timestamp) = sign_delete_with_keypair(&keypair1, &npub1, "reserved");
    app.clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"npub": npub1, "nym": "reserved", "signature": del_sig, "timestamp": del_timestamp}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    // User 2 tries to claim the same nym — should fail
    let (npub2, sig2, timestamp2) = sign_registration("reserved", TEST_DESCRIPTOR);
    let (_, body) = post_json(
        &app,
        "/register",
        json!({
            "nym": "reserved", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub2, "verification_npub": npub2, "signature": sig2, "timestamp": timestamp2,
        }),
    )
    .await;
    assert_eq!(body["status"], "ERROR");

    cleanup_db(&pool).await;
}

// --- Purge (destructive delete with reservation) ---

async fn delete_request(app: &Router, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/register")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn insert_swap(pool: &PgPool, nym: &str, status: &str, addr_idx: i32) {
    sqlx::query(
        "INSERT INTO swap_records \
         (nym, boltz_swap_id, address, address_index, amount_sat, invoice, \
          preimage_hex, claim_key_hex, boltz_response_json, status) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(nym)
    .bind(format!("boltz-{nym}-{addr_idx}"))
    .bind(format!("lq1addr{addr_idx}"))
    .bind(addr_idx)
    .bind(1000i64)
    .bind("lnbc10n1...")
    .bind("aa".repeat(32))
    .bind("bb".repeat(32))
    .bind("{}")
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

async fn create_test_user(pool: &PgPool, nym: &str) -> String {
    let (npub, _, _) = sign_registration(nym, TEST_DESCRIPTOR);
    pay_service::db::create_user(pool, nym, &npub, TEST_DESCRIPTOR)
        .await
        .unwrap();
    npub
}

async fn create_test_pos_page(pool: &PgPool, nym: &str, enabled: bool) {
    pay_service::db::upsert_donation_page(
        pool,
        &pay_service::db::UpsertDonationPage {
            nym,
            ct_descriptor: Some(TEST_DESCRIPTOR),
            header: "POS",
            description: "Point of sale",
            display_currency: "USD",
            website: None,
            twitter: None,
            instagram: None,
            enabled,
        },
    )
    .await
    .unwrap();
}

async fn create_claimed_terminal(
    pool: &PgPool,
    nym: &str,
    npub: &str,
    token: &str,
) -> pay_service::db::PosTerminal {
    let token_hash = hex::encode(Sha256::digest(token.as_bytes()));
    let pairing_code_hash = hex::encode(Sha256::digest(format!("code-{nym}").as_bytes()));
    let pending =
        pay_service::db::insert_pos_pairing(pool, nym, &token_hash, &pairing_code_hash, 300)
            .await
            .unwrap();
    pay_service::db::claim_pos_pairing(
        pool,
        nym,
        pending.pairing_code_hash.as_ref().unwrap(),
        npub,
        Some("Counter"),
        TEST_POS_DESCRIPTOR,
    )
    .await
    .unwrap()
    .unwrap()
}

async fn insert_pos_invoice(
    pool: &PgPool,
    nym: &str,
    npub: &str,
    terminal_id: uuid::Uuid,
    liquid_address: &str,
    memo: Option<&str>,
) -> pay_service::db::Invoice {
    pay_service::db::insert_invoice(
        pool,
        &pay_service::db::NewInvoice {
            nym_owner: Some(nym),
            npub_owner: npub,
            origin: "checkout",
            fiat_amount_minor: None,
            fiat_currency: None,
            amount_sat: 2_000,
            rate_minor_per_btc: None,
            rate_lock_secs: 3_600,
            memo,
            terminal_id: Some(terminal_id),
            memo_public: memo.is_some(),
            recipient_label: None,
            public_description: None,
            invoice_number: None,
            accept_btc: false,
            accept_ln: false,
            accept_liquid: true,
            bitcoin_address: None,
            liquid_address: Some(liquid_address),
            liquid_blinding_key_hex: Some("99".repeat(32).as_str()),
            expires_in_secs: 3_600,
        },
    )
    .await
    .unwrap()
}

async fn insert_wallet_origin_invoice(
    pool: &PgPool,
    nym: &str,
    npub: &str,
    liquid_address: &str,
    memo: Option<&str>,
) -> pay_service::db::Invoice {
    pay_service::db::insert_invoice(
        pool,
        &pay_service::db::NewInvoice {
            nym_owner: Some(nym),
            npub_owner: npub,
            origin: "wallet",
            fiat_amount_minor: None,
            fiat_currency: None,
            amount_sat: 1_000,
            rate_minor_per_btc: None,
            rate_lock_secs: 3_600,
            memo,
            terminal_id: None,
            memo_public: false,
            recipient_label: None,
            public_description: None,
            invoice_number: None,
            accept_btc: false,
            accept_ln: false,
            accept_liquid: true,
            bitcoin_address: None,
            liquid_address: Some(liquid_address),
            liquid_blinding_key_hex: Some("ab".repeat(32).as_str()),
            expires_in_secs: 3_600,
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn pos_pairing_http_lifecycle_and_terminal_management() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let (npub, _, _, keypair) = sign_registration_with_keypair("poshttp", TEST_DESCRIPTOR);
    pay_service::db::create_user(&pool, "poshttp", &npub, TEST_DESCRIPTOR)
        .await
        .unwrap();
    let disabled_npub = create_test_user(&pool, "posdisabled").await;
    assert!(!disabled_npub.is_empty());
    let archived_npub = create_test_user(&pool, "posarchived").await;
    assert!(!archived_npub.is_empty());
    create_test_pos_page(&pool, "poshttp", true).await;
    create_test_pos_page(&pool, "posdisabled", false).await;
    create_test_pos_page(&pool, "posarchived", true).await;
    sqlx::query("UPDATE donation_pages SET archived_at = NOW() WHERE nym = $1")
        .bind("posarchived")
        .execute(&pool)
        .await
        .unwrap();

    let token = "terminal-secret-token";
    let token_hash = hex::encode(Sha256::digest(token.as_bytes()));
    let (status, created) = post_json(
        &app,
        "/poshttp/pos/pairings",
        json!({ "token_hash": token_hash }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pairing_id = created["pairing_id"].as_str().unwrap().to_string();
    let code = created["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 8);

    let (wrong_poll_status, wrong_poll_body) = get_path(
        &app,
        &format!(
            "/poshttp/pos/pairings/{pairing_id}?token_hash={}",
            "f".repeat(64)
        ),
    )
    .await;
    assert_eq!(wrong_poll_status, StatusCode::OK);
    assert_eq!(wrong_poll_body["code"], "DonationPageNotFound");

    let (wrong_sig, wrong_ts) =
        sign_pos_pair_with_keypair(&keypair, &npub, "poshttp", "ZZZZZZZZ", "Front Counter");
    let (wrong_status, wrong_body) = post_json(
        &app,
        "/api/v1/pos/pairings/claim",
        json!({
            "npub": npub,
            "nym": "poshttp",
            "code": "ZZZZZZZZ",
            "label": "Front Counter",
            "pos_ct_descriptor": TEST_POS_DESCRIPTOR,
            "timestamp": wrong_ts,
            "signature": wrong_sig,
        }),
    )
    .await;
    assert_eq!(wrong_status, StatusCode::OK);
    assert_eq!(wrong_body["code"], "DonationPageNotFound");

    let (sig, ts) = sign_pos_pair_with_keypair(&keypair, &npub, "poshttp", &code, "Front Counter");
    let (claim_status, claim_body) = post_json(
        &app,
        "/api/v1/pos/pairings/claim",
        json!({
            "npub": npub,
            "nym": "poshttp",
            "code": code,
            "label": "Front Counter",
            "pos_ct_descriptor": TEST_POS_DESCRIPTOR,
            "timestamp": ts,
            "signature": sig,
        }),
    )
    .await;
    assert_eq!(claim_status, StatusCode::OK);
    let terminal_id = claim_body["terminal_id"].as_str().unwrap().to_string();

    let (poll_status, poll_body) = get_path(
        &app,
        &format!("/poshttp/pos/pairings/{pairing_id}?token_hash={token_hash}"),
    )
    .await;
    assert_eq!(poll_status, StatusCode::OK);
    assert_eq!(poll_body["status"], "approved");
    assert_eq!(poll_body["terminal_id"], terminal_id);
    assert!(poll_body.get("npub").is_none());
    assert!(poll_body.get("token_hash").is_none());

    let (double_sig, double_ts) =
        sign_pos_pair_with_keypair(&keypair, &npub, "poshttp", &code, "Front Counter");
    let (double_status, double_body) = post_json(
        &app,
        "/api/v1/pos/pairings/claim",
        json!({
            "npub": npub,
            "nym": "poshttp",
            "code": code,
            "label": "Front Counter",
            "pos_ct_descriptor": TEST_POS_DESCRIPTOR,
            "timestamp": double_ts,
            "signature": double_sig,
        }),
    )
    .await;
    assert_eq!(double_status, StatusCode::OK);
    assert_eq!(double_body["code"], "DonationPageNotFound");

    let (list_sig, list_ts) = sign_pos_terminal_list_with_keypair(&keypair, &npub);
    let (list_status, list_body) = get_path(
        &app,
        &format!("/api/v1/pos/terminals?npub={npub}&timestamp={list_ts}&signature={list_sig}"),
    )
    .await;
    assert_eq!(list_status, StatusCode::OK);
    assert_eq!(list_body["terminals"][0]["id"], terminal_id);
    assert!(list_body["terminals"][0].get("token_hash").is_none());
    assert!(list_body["terminals"][0].get("pairing_code_hash").is_none());

    let (revoke_sig, revoke_ts) =
        sign_pos_terminal_revoke_with_keypair(&keypair, &npub, &terminal_id);
    let (revoke_status, revoke_body) = post_json(
        &app,
        &format!("/api/v1/pos/terminals/{terminal_id}/revoke"),
        json!({ "npub": npub, "timestamp": revoke_ts, "signature": revoke_sig }),
    )
    .await;
    assert_eq!(revoke_status, StatusCode::OK);
    assert_eq!(revoke_body["revoked"], true);
    assert!(
        pay_service::db::get_active_terminal_by_token(&pool, "poshttp", &token_hash)
            .await
            .unwrap()
            .is_none()
    );

    let (disabled_status, disabled_body) = post_json(
        &app,
        "/posdisabled/pos/pairings",
        json!({ "token_hash": "a".repeat(64) }),
    )
    .await;
    assert_eq!(disabled_status, StatusCode::OK);
    assert!(disabled_body["pairing_id"].as_str().is_some());

    let (missing_status, missing_body) = post_json(
        &app,
        "/posmissing/pos/pairings",
        json!({ "token_hash": "a".repeat(64) }),
    )
    .await;
    assert_eq!(missing_status, StatusCode::OK);
    assert_eq!(missing_body["code"], "DonationPageNotFound");

    let (archived_status, archived_body) = post_json(
        &app,
        "/posarchived/pos/pairings",
        json!({ "token_hash": "a".repeat(64) }),
    )
    .await;
    assert_eq!(archived_status, StatusCode::OK);
    assert_eq!(archived_body["code"], "DonationPageNotFound");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_flow_works_for_active_nym_without_page() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let (npub, _, _, keypair) = sign_registration_with_keypair("posnopage", TEST_DESCRIPTOR);
    pay_service::db::create_user(&pool, "posnopage", &npub, TEST_DESCRIPTOR)
        .await
        .unwrap();

    // POS shell and manifest render for an active nym with no page row.
    let (shell_status, _, shell_html) = get_text_with_headers(&app, "/posnopage/pos").await;
    assert_eq!(shell_status, StatusCode::OK);
    assert!(!shell_html.is_empty());
    let (manifest_status, _, manifest_body) =
        get_json_with_headers(&app, "/posnopage/pos/manifest.webmanifest").await;
    assert_eq!(manifest_status, StatusCode::OK);
    assert_eq!(manifest_body["start_url"], "/posnopage/pos");

    // The public donation surface stays hidden.
    let (public_status, _, _) = get_text_with_headers(&app, "/posnopage").await;
    assert_eq!(public_status, StatusCode::NOT_FOUND);

    // Pairing creation gates on the active nym, not a page row.
    let token = "posnopage-token";
    let token_hash = hex::encode(Sha256::digest(token.as_bytes()));
    let (status, created) = post_json(
        &app,
        "/posnopage/pos/pairings",
        json!({ "token_hash": token_hash }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let code = created["code"].as_str().unwrap().to_string();

    // The signed claim materializes the disabled placeholder row and stores
    // the POS descriptor on it.
    let (sig, ts) = sign_pos_pair_with_keypair(&keypair, &npub, "posnopage", &code, "Counter");
    let (claim_status, claim_body) = post_json(
        &app,
        "/api/v1/pos/pairings/claim",
        json!({
            "npub": npub,
            "nym": "posnopage",
            "code": code,
            "label": "Counter",
            "pos_ct_descriptor": TEST_POS_DESCRIPTOR,
            "timestamp": ts,
            "signature": sig,
        }),
    )
    .await;
    assert_eq!(claim_status, StatusCode::OK);
    assert!(claim_body["terminal_id"].as_str().is_some());

    let (enabled, archived, stored): (bool, bool, Option<String>) = sqlx::query_as(
        "SELECT enabled, archived_at IS NOT NULL, pos_ct_descriptor \
         FROM donation_pages WHERE nym = $1",
    )
    .bind("posnopage")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!enabled);
    assert!(!archived);
    assert_eq!(stored.as_deref(), Some(TEST_POS_DESCRIPTOR));

    // Third state: disabled placeholder + active POS profile. The public
    // page and anonymous checkout stay hidden after the claim.
    let (public_status, _, _) = get_text_with_headers(&app, "/posnopage").await;
    assert_eq!(public_status, StatusCode::NOT_FOUND);
    let (anon_status, anon_body) =
        post_json(&app, "/posnopage/invoice", json!({ "amount_sat": 1000 })).await;
    assert_eq!(anon_status, StatusCode::OK);
    assert_eq!(anon_body["code"], "DonationPageNotFound");

    // Terminal invoices allocate from the POS descriptor on the placeholder.
    let bearer = format!("Bearer {token}");
    let (inv_status, inv_body) = post_json_with_headers(
        &app,
        "/posnopage/pos/invoice",
        json!({ "amount_sat": 1000, "memo": "counter sale" }),
        &[("authorization", bearer.as_str())],
    )
    .await;
    assert_eq!(inv_status, StatusCode::OK);
    assert_ne!(inv_body["code"], "DonationPageNotFound");
    let pos_cursor: i32 =
        sqlx::query_scalar("SELECT pos_next_addr_idx FROM donation_pages WHERE nym = $1")
            .bind("posnopage")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pos_cursor, 1);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_pairing_claim_sets_or_matches_pos_descriptor() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let (npub, _, _, keypair) = sign_registration_with_keypair("poswallet", TEST_DESCRIPTOR);
    pay_service::db::create_user(&pool, "poswallet", &npub, TEST_DESCRIPTOR)
        .await
        .unwrap();
    create_test_pos_page(&pool, "poswallet", true).await;

    for (code, token_hash) in [("ABCDEFGH", "1".repeat(64)), ("BCDEFGHJ", "2".repeat(64))] {
        pay_service::db::insert_pos_pairing(
            &pool,
            "poswallet",
            &token_hash,
            &hex::encode(Sha256::digest(code.as_bytes())),
            300,
        )
        .await
        .unwrap();
        let (sig, ts) = sign_pos_pair_with_keypair(&keypair, &npub, "poswallet", code, "Counter");
        let (status, body) = post_json(
            &app,
            "/api/v1/pos/pairings/claim",
            json!({
                "npub": npub,
                "nym": "poswallet",
                "code": code,
                "label": "Counter",
                "pos_ct_descriptor": TEST_POS_DESCRIPTOR,
                "timestamp": ts,
                "signature": sig,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:?}");
        assert!(body["terminal_id"].as_str().is_some());
    }

    let stored: String =
        sqlx::query_scalar("SELECT pos_ct_descriptor FROM donation_pages WHERE nym = $1")
            .bind("poswallet")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, TEST_POS_DESCRIPTOR);

    let bad_code = "CDEFGHJK";
    pay_service::db::insert_pos_pairing(
        &pool,
        "poswallet",
        &"3".repeat(64),
        &hex::encode(Sha256::digest(bad_code.as_bytes())),
        300,
    )
    .await
    .unwrap();
    let (bad_sig, bad_ts) = sign_la_action(
        &keypair,
        "pos-pair",
        &npub,
        "poswallet",
        &[bad_code, "Counter", OTHER_TEST_POS_DESCRIPTOR],
    );
    let (bad_status, bad_body) = post_json(
        &app,
        "/api/v1/pos/pairings/claim",
        json!({
            "npub": npub,
            "nym": "poswallet",
            "code": bad_code,
            "label": "Counter",
            "pos_ct_descriptor": OTHER_TEST_POS_DESCRIPTOR,
            "timestamp": bad_ts,
            "signature": bad_sig,
        }),
    )
    .await;
    assert_eq!(bad_status, StatusCode::CONFLICT);
    assert_eq!(bad_body["code"], "PosDescriptorMismatch");

    let stored_after: String =
        sqlx::query_scalar("SELECT pos_ct_descriptor FROM donation_pages WHERE nym = $1")
            .bind("poswallet")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored_after, TEST_POS_DESCRIPTOR);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_pairing_claim_validates_descriptor_after_signature_check() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let (npub, _, _, keypair) = sign_registration_with_keypair("posdesc", TEST_DESCRIPTOR);
    pay_service::db::create_user(&pool, "posdesc", &npub, TEST_DESCRIPTOR)
        .await
        .unwrap();
    create_test_pos_page(&pool, "posdesc", true).await;

    for (idx, (code, descriptor)) in [("ABCDEFGH", ""), ("BCDEFGHJ", "not-a-descriptor")]
        .into_iter()
        .enumerate()
    {
        pay_service::db::insert_pos_pairing(
            &pool,
            "posdesc",
            &format!("{:x}", idx + 1).repeat(64),
            &hex::encode(Sha256::digest(code.as_bytes())),
            300,
        )
        .await
        .unwrap();
        let (sig, ts) = sign_la_action(
            &keypair,
            "pos-pair",
            &npub,
            "posdesc",
            &[code, "Counter", descriptor],
        );
        let (status, body) = post_json(
            &app,
            "/api/v1/pos/pairings/claim",
            json!({
                "npub": npub,
                "nym": "posdesc",
                "code": code,
                "label": "Counter",
                "pos_ct_descriptor": descriptor,
                "timestamp": ts,
                "signature": sig,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "InvalidDescriptor");
    }

    let code = "CDEFGHJK";
    pay_service::db::insert_pos_pairing(
        &pool,
        "posdesc",
        &"f".repeat(64),
        &hex::encode(Sha256::digest(code.as_bytes())),
        300,
    )
    .await
    .unwrap();
    let (sig, ts) = sign_pos_pair_with_keypair(&keypair, &npub, "posdesc", code, "Counter");
    let (status, body) = post_json(
        &app,
        "/api/v1/pos/pairings/claim",
        json!({
            "npub": npub,
            "nym": "posdesc",
            "code": code,
            "label": "Counter",
            "pos_ct_descriptor": OTHER_TEST_POS_DESCRIPTOR,
            "timestamp": ts,
            "signature": sig,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "AuthError");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_pairing_claim_after_expiry_fails_uniformly() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let (npub, _, _, keypair) = sign_registration_with_keypair("posexpired", TEST_DESCRIPTOR);
    pay_service::db::create_user(&pool, "posexpired", &npub, TEST_DESCRIPTOR)
        .await
        .unwrap();
    let code = "ABCDEFGH";
    pay_service::db::insert_pos_pairing(
        &pool,
        "posexpired",
        &"b".repeat(64),
        &hex::encode(Sha256::digest(code.as_bytes())),
        -1,
    )
    .await
    .unwrap();

    let (sig, ts) = sign_pos_pair_with_keypair(&keypair, &npub, "posexpired", code, "");
    let (status, body) = post_json(
        &app,
        "/api/v1/pos/pairings/claim",
        json!({
            "npub": npub,
            "nym": "posexpired",
            "code": code,
            "label": "",
            "pos_ct_descriptor": TEST_POS_DESCRIPTOR,
            "timestamp": ts,
            "signature": sig,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["code"], "DonationPageNotFound");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_pairing_failed_claims_are_rate_limited() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let mut cfg = test_config();
    cfg.rate_limit.trust_forwarded_for = true;
    cfg.rate_limit
        .pos_pairing_claim_failures_per_source_per_hour = 1;
    let app = test_app(test_state_with_config(pool.clone(), cfg));
    let (npub, _, _, keypair) = sign_registration_with_keypair("poslimit", TEST_DESCRIPTOR);
    pay_service::db::create_user(&pool, "poslimit", &npub, TEST_DESCRIPTOR)
        .await
        .unwrap();

    let bad_code = "BADCODE0";
    let (bad_sig, bad_ts) = sign_pos_pair_with_keypair(&keypair, &npub, "poslimit", bad_code, "");
    let source_headers = [("x-forwarded-for", "203.0.113.7")];
    let (bad_status, bad_body) = post_json_with_headers(
        &app,
        "/api/v1/pos/pairings/claim",
        json!({
            "npub": npub,
            "nym": "poslimit",
            "code": bad_code,
            "label": "",
            "pos_ct_descriptor": TEST_POS_DESCRIPTOR,
            "timestamp": bad_ts,
            "signature": bad_sig,
        }),
        &source_headers,
    )
    .await;
    assert_eq!(bad_status, StatusCode::OK);
    assert_eq!(bad_body["code"], "DonationPageNotFound");

    let correct_code = "ABCDEFGH";
    let pending = pay_service::db::insert_pos_pairing(
        &pool,
        "poslimit",
        &"d".repeat(64),
        &hex::encode(Sha256::digest(correct_code.as_bytes())),
        300,
    )
    .await
    .unwrap();
    let (good_sig, good_ts) =
        sign_pos_pair_with_keypair(&keypair, &npub, "poslimit", correct_code, "");
    let (blocked_status, blocked_body) = post_json_with_headers(
        &app,
        "/api/v1/pos/pairings/claim",
        json!({
            "npub": npub,
            "nym": "poslimit",
            "code": correct_code,
            "label": "",
            "pos_ct_descriptor": TEST_POS_DESCRIPTOR,
            "timestamp": good_ts,
            "signature": good_sig,
        }),
        &source_headers,
    )
    .await;
    assert_eq!(blocked_status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(blocked_body["code"], "PosPairingClaimRateLimited");
    let still_pending = pay_service::db::get_pos_pairing(&pool, pending.id, &"d".repeat(64))
        .await
        .unwrap()
        .unwrap();
    assert!(still_pending.claimed_at_unix.is_none());

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_pairing_db_lifecycle_enforces_claim_and_revocation_rules() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "poslife").await;
    create_test_pos_page(&pool, "poslife", true).await;
    let token_hash = "a".repeat(64);
    let code_hash = "b".repeat(64);

    let pending =
        pay_service::db::insert_pos_pairing(&pool, "poslife", &token_hash, &code_hash, 300)
            .await
            .unwrap();
    assert_eq!(pending.nym, "poslife");
    assert_eq!(pending.npub_owner, None);
    assert_eq!(pending.claimed_at_unix, None);

    let claimed = pay_service::db::claim_pos_pairing(
        &pool,
        "poslife",
        &code_hash,
        &npub,
        Some("Front register"),
        TEST_POS_DESCRIPTOR,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(claimed.id, pending.id);
    assert_eq!(claimed.npub_owner.as_deref(), Some(npub.as_str()));
    assert_eq!(claimed.label.as_deref(), Some("Front register"));
    assert!(claimed.claimed_at_unix.is_some());
    assert_eq!(claimed.pairing_code_hash, None);

    let fetched = pay_service::db::get_pos_pairing(&pool, pending.id, &token_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.id, pending.id);

    let double_claim = pay_service::db::claim_pos_pairing(
        &pool,
        "poslife",
        &code_hash,
        &npub,
        Some("Again"),
        TEST_POS_DESCRIPTOR,
    )
    .await
    .unwrap();
    assert!(double_claim.is_none());

    let active = pay_service::db::get_active_terminal_by_token(&pool, "poslife", &token_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active.id, pending.id);

    assert_eq!(
        pay_service::db::touch_terminal_seen(&pool, pending.id)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        pay_service::db::touch_terminal_seen(&pool, pending.id)
            .await
            .unwrap(),
        0
    );
    sqlx::query(
        "UPDATE pos_terminals SET last_seen_at = NOW() - INTERVAL '61 seconds' WHERE id = $1",
    )
    .bind(pending.id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        pay_service::db::touch_terminal_seen(&pool, pending.id)
            .await
            .unwrap(),
        1
    );

    assert_eq!(
        pay_service::db::revoke_terminal(&pool, pending.id, &npub)
            .await
            .unwrap(),
        1
    );
    let revoked = pay_service::db::get_active_terminal_by_token(&pool, "poslife", &token_hash)
        .await
        .unwrap();
    assert!(revoked.is_none());

    let expired =
        pay_service::db::insert_pos_pairing(&pool, "poslife", &"c".repeat(64), &"d".repeat(64), -1)
            .await
            .unwrap();
    let expired_claim = pay_service::db::claim_pos_pairing(
        &pool,
        "poslife",
        expired.pairing_code_hash.as_ref().unwrap(),
        &npub,
        None,
        TEST_POS_DESCRIPTOR,
    )
    .await
    .unwrap();
    assert!(expired_claim.is_none());

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_pairing_claim_is_bound_to_pairing_nym() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let alice_npub = create_test_user(&pool, "posalice").await;
    let bob_npub = create_test_user(&pool, "posbob").await;
    create_test_pos_page(&pool, "posalice", true).await;
    create_test_pos_page(&pool, "posbob", true).await;
    let token_hash = "e".repeat(64);
    let code_hash = "f".repeat(64);

    let pending =
        pay_service::db::insert_pos_pairing(&pool, "posalice", &token_hash, &code_hash, 300)
            .await
            .unwrap();

    let wrong_nym_claim = pay_service::db::claim_pos_pairing(
        &pool,
        "posbob",
        &code_hash,
        &bob_npub,
        Some("Bob"),
        TEST_POS_DESCRIPTOR,
    )
    .await
    .unwrap();
    assert!(wrong_nym_claim.is_none());

    let still_pending = pay_service::db::get_pos_pairing(&pool, pending.id, &token_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still_pending.nym, "posalice");
    assert_eq!(still_pending.npub_owner, None);
    assert_eq!(still_pending.claimed_at_unix, None);
    assert_eq!(
        still_pending.pairing_code_hash.as_deref(),
        Some(code_hash.as_str())
    );

    let claimed = pay_service::db::claim_pos_pairing(
        &pool,
        "posalice",
        &code_hash,
        &alice_npub,
        Some("Alice"),
        TEST_POS_DESCRIPTOR,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(claimed.id, pending.id);
    assert_eq!(claimed.npub_owner.as_deref(), Some(alice_npub.as_str()));

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_invoice_fields_round_trip_and_pos_list_filters_by_nym() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "posinvoices").await;
    let other_npub = create_test_user(&pool, "posother").await;
    create_test_pos_page(&pool, "posinvoices", true).await;
    create_test_pos_page(&pool, "posother", true).await;
    let terminal = pay_service::db::claim_pos_pairing(
        &pool,
        "posinvoices",
        &pay_service::db::insert_pos_pairing(
            &pool,
            "posinvoices",
            &"1".repeat(64),
            &"2".repeat(64),
            300,
        )
        .await
        .unwrap()
        .pairing_code_hash
        .unwrap(),
        &npub,
        Some("Counter"),
        TEST_POS_DESCRIPTOR,
    )
    .await
    .unwrap()
    .unwrap();
    let other_terminal = pay_service::db::claim_pos_pairing(
        &pool,
        "posother",
        &pay_service::db::insert_pos_pairing(
            &pool,
            "posother",
            &"3".repeat(64),
            &"4".repeat(64),
            300,
        )
        .await
        .unwrap()
        .pairing_code_hash
        .unwrap(),
        &other_npub,
        None,
        TEST_POS_DESCRIPTOR,
    )
    .await
    .unwrap()
    .unwrap();

    let pos_invoice = pay_service::db::insert_invoice(
        &pool,
        &pay_service::db::NewInvoice {
            nym_owner: Some("posinvoices"),
            npub_owner: &npub,
            origin: "checkout",
            fiat_amount_minor: None,
            fiat_currency: None,
            amount_sat: 2_000,
            rate_minor_per_btc: None,
            rate_lock_secs: 3_600,
            memo: Some("Two coffees"),
            terminal_id: Some(terminal.id),
            memo_public: true,
            recipient_label: None,
            public_description: None,
            invoice_number: None,
            accept_btc: false,
            accept_ln: false,
            accept_liquid: true,
            bitcoin_address: None,
            liquid_address: Some("lq1posinvoices"),
            liquid_blinding_key_hex: Some("33".repeat(32).as_str()),
            expires_in_secs: 3_600,
        },
    )
    .await
    .unwrap();
    let _wallet_invoice =
        insert_test_invoice(&pool, "posinvoices", &npub, "lq1posinvoiceswallet", 3_600).await;
    let _other_pos_invoice = pay_service::db::insert_invoice(
        &pool,
        &pay_service::db::NewInvoice {
            nym_owner: Some("posother"),
            npub_owner: &other_npub,
            origin: "checkout",
            fiat_amount_minor: None,
            fiat_currency: None,
            amount_sat: 3_000,
            rate_minor_per_btc: None,
            rate_lock_secs: 3_600,
            memo: Some("Other counter"),
            terminal_id: Some(other_terminal.id),
            memo_public: true,
            recipient_label: None,
            public_description: None,
            invoice_number: None,
            accept_btc: false,
            accept_ln: false,
            accept_liquid: true,
            bitcoin_address: None,
            liquid_address: Some("lq1posother"),
            liquid_blinding_key_hex: Some("44".repeat(32).as_str()),
            expires_in_secs: 3_600,
        },
    )
    .await
    .unwrap();

    let fetched = pay_service::db::get_invoice_by_id(&pool, pos_invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.terminal_id, Some(terminal.id));
    assert!(fetched.memo_public);
    assert_eq!(fetched.memo.as_deref(), Some("Two coffees"));

    let rows = pay_service::db::list_pos_invoices_by_nym(&pool, "posinvoices", None, 1, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, pos_invoice.id);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_invoice_constraints_match_terminal_metadata_policy() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "posconstraints").await;
    create_test_pos_page(&pool, "posconstraints", true).await;
    let pending = pay_service::db::insert_pos_pairing(
        &pool,
        "posconstraints",
        &"5".repeat(64),
        &"6".repeat(64),
        300,
    )
    .await
    .unwrap();
    let terminal = pay_service::db::claim_pos_pairing(
        &pool,
        "posconstraints",
        pending.pairing_code_hash.as_ref().unwrap(),
        &npub,
        None,
        TEST_POS_DESCRIPTOR,
    )
    .await
    .unwrap()
    .unwrap();

    let anon_metadata = pay_service::db::insert_invoice(
        &pool,
        &pay_service::db::NewInvoice {
            nym_owner: Some("posconstraints"),
            npub_owner: &npub,
            origin: "checkout",
            fiat_amount_minor: None,
            fiat_currency: None,
            amount_sat: 1_000,
            rate_minor_per_btc: None,
            rate_lock_secs: 3_600,
            memo: Some("anonymous memo"),
            terminal_id: None,
            memo_public: false,
            recipient_label: None,
            public_description: None,
            invoice_number: None,
            accept_btc: false,
            accept_ln: false,
            accept_liquid: true,
            bitcoin_address: None,
            liquid_address: Some("lq1anonmetadata"),
            liquid_blinding_key_hex: Some("55".repeat(32).as_str()),
            expires_in_secs: 3_600,
        },
    )
    .await;
    assert!(anon_metadata.is_err());

    let terminal_memo = pay_service::db::insert_invoice(
        &pool,
        &pay_service::db::NewInvoice {
            nym_owner: Some("posconstraints"),
            npub_owner: &npub,
            origin: "checkout",
            fiat_amount_minor: None,
            fiat_currency: None,
            amount_sat: 1_000,
            rate_minor_per_btc: None,
            rate_lock_secs: 3_600,
            memo: Some("terminal memo"),
            terminal_id: Some(terminal.id),
            memo_public: true,
            recipient_label: None,
            public_description: None,
            invoice_number: None,
            accept_btc: false,
            accept_ln: false,
            accept_liquid: true,
            bitcoin_address: None,
            liquid_address: Some("lq1terminalmemo"),
            liquid_blinding_key_hex: Some("66".repeat(32).as_str()),
            expires_in_secs: 3_600,
        },
    )
    .await;
    assert!(terminal_memo.is_ok());

    let terminal_label = pay_service::db::insert_invoice(
        &pool,
        &pay_service::db::NewInvoice {
            nym_owner: Some("posconstraints"),
            npub_owner: &npub,
            origin: "checkout",
            fiat_amount_minor: None,
            fiat_currency: None,
            amount_sat: 1_000,
            rate_minor_per_btc: None,
            rate_lock_secs: 3_600,
            memo: Some("terminal memo"),
            terminal_id: Some(terminal.id),
            memo_public: true,
            recipient_label: Some("blocked"),
            public_description: None,
            invoice_number: None,
            accept_btc: false,
            accept_ln: false,
            accept_liquid: true,
            bitcoin_address: None,
            liquid_address: Some("lq1terminallabel"),
            liquid_blinding_key_hex: Some("77".repeat(32).as_str()),
            expires_in_secs: 3_600,
        },
    )
    .await;
    assert!(terminal_label.is_err());

    let wallet_public_memo = pay_service::db::insert_invoice(
        &pool,
        &pay_service::db::NewInvoice {
            nym_owner: Some("posconstraints"),
            npub_owner: &npub,
            origin: "wallet",
            fiat_amount_minor: None,
            fiat_currency: None,
            amount_sat: 1_000,
            rate_minor_per_btc: None,
            rate_lock_secs: 3_600,
            memo: None,
            terminal_id: None,
            memo_public: true,
            recipient_label: None,
            public_description: None,
            invoice_number: None,
            accept_btc: false,
            accept_ln: false,
            accept_liquid: true,
            bitcoin_address: None,
            liquid_address: Some("lq1walletpublicmemo"),
            liquid_blinding_key_hex: Some("88".repeat(32).as_str()),
            expires_in_secs: 3_600,
        },
    )
    .await;
    assert!(wallet_public_memo.is_err());

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_terminal_invoice_routes_require_active_bearer() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let npub = create_test_user(&pool, "posauth").await;
    create_test_pos_page(&pool, "posauth", true).await;
    let token = "posauth-token";
    let terminal = create_claimed_terminal(&pool, "posauth", &npub, token).await;
    let bearer = format!("Bearer {token}");

    let (missing_status, missing_body) =
        get_path(&app, "/posauth/pos/invoices?page=1&pageSize=10").await;
    assert_eq!(missing_status, StatusCode::UNAUTHORIZED);
    assert_eq!(missing_body["code"], "AuthError");

    let (bad_status, bad_body) = get_path_with_headers(
        &app,
        "/posauth/pos/invoices?page=1&pageSize=10",
        &[("authorization", "Bearer garbage")],
    )
    .await;
    assert_eq!(bad_status, StatusCode::UNAUTHORIZED);
    assert_eq!(bad_body["code"], "AuthError");

    let (valid_status, valid_body) = get_path_with_headers(
        &app,
        "/posauth/pos/invoices?page=1&pageSize=10",
        &[("authorization", bearer.as_str())],
    )
    .await;
    assert_eq!(valid_status, StatusCode::OK, "{valid_body:?}");
    assert_eq!(valid_body["invoices"].as_array().unwrap().len(), 0);

    let (bad_create_status, bad_create_body) = post_json_with_headers(
        &app,
        "/posauth/pos/invoice",
        json!({ "amount_sat": 1000, "memo": "safe" }),
        &[("authorization", "Bearer garbage")],
    )
    .await;
    assert_eq!(bad_create_status, StatusCode::UNAUTHORIZED);
    assert_eq!(bad_create_body["code"], "AuthError");

    pay_service::db::revoke_terminal(&pool, terminal.id, &npub)
        .await
        .unwrap();
    let (revoked_status, revoked_body) = get_path_with_headers(
        &app,
        "/posauth/pos/invoices?page=1&pageSize=10",
        &[("authorization", bearer.as_str())],
    )
    .await;
    assert_eq!(revoked_status, StatusCode::UNAUTHORIZED);
    assert_eq!(revoked_body["code"], "AuthError");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_terminal_create_rejects_invalid_memo_before_invoice_insert() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let npub = create_test_user(&pool, "posmemo").await;
    create_test_pos_page(&pool, "posmemo", true).await;
    let token = "posmemo-token";
    create_claimed_terminal(&pool, "posmemo", &npub, token).await;
    let bearer = format!("Bearer {token}");

    for memo in ["a".repeat(281), "line one\nline two".to_string()] {
        let (status, body) = post_json_with_headers(
            &app,
            "/posmemo/pos/invoice",
            json!({ "amount_sat": 1000, "memo": memo }),
            &[("authorization", bearer.as_str())],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "InvalidAmount");
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM invoices WHERE nym_owner = $1")
        .bind("posmemo")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_terminal_invoice_on_disabled_page_passes_page_gate() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let npub = create_test_user(&pool, "posdisabledinvoice").await;
    create_test_pos_page(&pool, "posdisabledinvoice", false).await;
    sqlx::query("UPDATE donation_pages SET ct_descriptor = 'not-a-descriptor' WHERE nym = $1")
        .bind("posdisabledinvoice")
        .execute(&pool)
        .await
        .unwrap();
    let token = "posdisabledinvoice-token";
    create_claimed_terminal(&pool, "posdisabledinvoice", &npub, token).await;
    let bearer = format!("Bearer {token}");

    let (status, body) = post_json_with_headers(
        &app,
        "/posdisabledinvoice/pos/invoice",
        json!({ "amount_sat": 1000, "memo": "counter sale" }),
        &[("authorization", bearer.as_str())],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(body["code"], "DonationPageNotFound");

    let cursors: (i32, i32) = sqlx::query_as(
        "SELECT next_addr_idx, pos_next_addr_idx FROM donation_pages WHERE nym = $1",
    )
    .bind("posdisabledinvoice")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cursors.0, 0);
    assert_eq!(cursors.1, 1);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM invoices WHERE nym_owner = $1")
        .bind("posdisabledinvoice")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_terminal_invoice_requires_pos_descriptor_without_donation_fallback() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let npub = create_test_user(&pool, "posmissingdesc").await;
    create_test_pos_page(&pool, "posmissingdesc", true).await;
    let token = "posmissingdesc-token";
    create_claimed_terminal(&pool, "posmissingdesc", &npub, token).await;
    sqlx::query(
        "UPDATE donation_pages \
         SET pos_ct_descriptor = NULL, pos_next_addr_idx = 0, next_addr_idx = 0 \
         WHERE nym = $1",
    )
    .bind("posmissingdesc")
    .execute(&pool)
    .await
    .unwrap();
    let bearer = format!("Bearer {token}");

    let (status, body) = post_json_with_headers(
        &app,
        "/posmissingdesc/pos/invoice",
        json!({ "amount_sat": 1000, "memo": "counter sale" }),
        &[("authorization", bearer.as_str())],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["code"], "PosDescriptorRequired");

    let cursors: (i32, i32) = sqlx::query_as(
        "SELECT next_addr_idx, pos_next_addr_idx FROM donation_pages WHERE nym = $1",
    )
    .bind("posmissingdesc")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cursors, (0, 0));

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn anonymous_invoice_create_rejects_disabled_pages_only() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    create_test_user(&pool, "anondisabled").await;
    create_test_pos_page(&pool, "anondisabled", false).await;
    create_test_user(&pool, "anondonation").await;
    create_test_pos_page(&pool, "anondonation", true).await;
    sqlx::query("UPDATE donation_pages SET ct_descriptor = 'not-a-descriptor' WHERE nym = $1")
        .bind("anondonation")
        .execute(&pool)
        .await
        .unwrap();

    let (disabled_status, disabled_body) =
        post_json(&app, "/anondisabled/invoice", json!({ "amount_sat": 1000 })).await;
    assert_eq!(disabled_status, StatusCode::OK);
    assert_eq!(disabled_body["code"], "DonationPageNotFound");

    let (donation_status, donation_body) =
        post_json(&app, "/anondonation/invoice", json!({ "amount_sat": 1000 })).await;
    assert_eq!(donation_status, StatusCode::OK);
    assert_ne!(donation_body["code"], "DonationPageNotFound");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_terminal_invoice_list_filters_sorts_and_clamps() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let npub = create_test_user(&pool, "poslist").await;
    let other_npub = create_test_user(&pool, "poslistother").await;
    create_test_pos_page(&pool, "poslist", true).await;
    create_test_pos_page(&pool, "poslistother", true).await;
    let token = "poslist-token";
    let terminal = create_claimed_terminal(&pool, "poslist", &npub, token).await;
    let other_terminal =
        create_claimed_terminal(&pool, "poslistother", &other_npub, "other-token").await;
    let bearer = format!("Bearer {token}");

    let old = insert_pos_invoice(
        &pool,
        "poslist",
        &npub,
        terminal.id,
        "lq1poslistold",
        Some("Old sale"),
    )
    .await;
    let newest = insert_pos_invoice(
        &pool,
        "poslist",
        &npub,
        terminal.id,
        "lq1poslistnew",
        Some("Newest sale"),
    )
    .await;
    sqlx::query("UPDATE invoices SET created_at = NOW() - INTERVAL '1 hour' WHERE id = $1")
        .bind(old.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE invoices SET created_at = NOW() + INTERVAL '1 hour' WHERE id = $1")
        .bind(newest.id)
        .execute(&pool)
        .await
        .unwrap();
    let paid = insert_pos_invoice(
        &pool,
        "poslist",
        &npub,
        terminal.id,
        "lq1poslistpaid",
        Some("Paid sale"),
    )
    .await;
    sqlx::query("UPDATE invoices SET status = 'paid', paid_via = 'liquid', paid_at = NOW(), paid_amount_sat = amount_sat WHERE id = $1")
        .bind(paid.id)
        .execute(&pool)
        .await
        .unwrap();
    let wallet =
        insert_wallet_origin_invoice(&pool, "poslist", &npub, "lq1poslistwallet", None).await;
    let _legacy_checkout =
        insert_test_invoice(&pool, "poslist", &npub, "lq1poslistlegacy", 3_600).await;
    let _foreign = insert_pos_invoice(
        &pool,
        "poslistother",
        &other_npub,
        other_terminal.id,
        "lq1poslistforeign",
        Some("Foreign"),
    )
    .await;
    for i in 0..101 {
        let address = format!("lq1poslistbulk{i}");
        insert_pos_invoice(&pool, "poslist", &npub, terminal.id, &address, Some("Bulk")).await;
    }

    let (status, body) = get_path_with_headers(
        &app,
        "/poslist/pos/invoices?page=1&pageSize=10",
        &[("authorization", bearer.as_str())],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["invoices"][0]["id"], newest.id.to_string());
    assert_eq!(body["invoices"][0]["memo"], "Newest sale");
    assert_eq!(body["invoices"][0]["terminal_id"], terminal.id.to_string());
    assert!(body["invoices"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| { item["terminal_id"] == terminal.id.to_string() }));
    assert!(body["invoices"].as_array().unwrap().iter().all(|item| {
        item["id"] != wallet.id.to_string()
            && item["id"] != _legacy_checkout.id.to_string()
            && item["id"] != _foreign.id.to_string()
    }));

    let (paid_status, paid_body) = get_path_with_headers(
        &app,
        "/poslist/pos/invoices?page=1&pageSize=10&status=paid",
        &[("authorization", bearer.as_str())],
    )
    .await;
    assert_eq!(paid_status, StatusCode::OK, "{paid_body:?}");
    assert_eq!(paid_body["invoices"].as_array().unwrap().len(), 1);
    assert_eq!(paid_body["invoices"][0]["id"], paid.id.to_string());

    let (clamp_status, clamp_body) = get_path_with_headers(
        &app,
        "/poslist/pos/invoices?page=1&pageSize=1000",
        &[("authorization", bearer.as_str())],
    )
    .await;
    assert_eq!(clamp_status, StatusCode::OK, "{clamp_body:?}");
    assert_eq!(clamp_body["pageSize"], 100);
    assert_eq!(clamp_body["invoices"].as_array().unwrap().len(), 100);
    assert_eq!(clamp_body["has_more"], true);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn pos_terminal_cancel_semantics_are_uniform_and_distinct() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let npub = create_test_user(&pool, "poscancel").await;
    let other_npub = create_test_user(&pool, "poscancelforeign").await;
    create_test_pos_page(&pool, "poscancel", true).await;
    create_test_pos_page(&pool, "poscancelforeign", true).await;
    let token = "poscancel-token";
    let terminal = create_claimed_terminal(&pool, "poscancel", &npub, token).await;
    let other_terminal =
        create_claimed_terminal(&pool, "poscancelforeign", &other_npub, "foreign-token").await;
    let bearer = format!("Bearer {token}");

    let unpaid = insert_pos_invoice(
        &pool,
        "poscancel",
        &npub,
        terminal.id,
        "lq1poscancelunpaid",
        Some("Cancel me"),
    )
    .await;
    let (status, body) = post_json_with_headers(
        &app,
        &format!("/poscancel/pos/invoices/{}/cancel", unpaid.id),
        json!({}),
        &[("authorization", bearer.as_str())],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["status"], "cancelled");

    for (suffix, invoice_status) in [
        ("progress", "in_progress"),
        ("partial", "partially_paid"),
        ("paid", "paid"),
    ] {
        let inv = insert_pos_invoice(
            &pool,
            "poscancel",
            &npub,
            terminal.id,
            &format!("lq1poscancel{suffix}"),
            Some("Already seen"),
        )
        .await;
        match invoice_status {
            "in_progress" => {
                sqlx::query("UPDATE invoices SET status = 'in_progress' WHERE id = $1")
                    .bind(inv.id)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            "partially_paid" => {
                sqlx::query(
                    "UPDATE invoices \
                     SET status = 'partially_paid', paid_via = 'liquid', paid_amount_sat = 400 \
                     WHERE id = $1",
                )
                .bind(inv.id)
                .execute(&pool)
                .await
                .unwrap();
            }
            "paid" => {
                sqlx::query(
                    "UPDATE invoices \
                     SET status = 'paid', paid_via = 'liquid', paid_amount_sat = amount_sat, \
                         paid_at = NOW() \
                     WHERE id = $1",
                )
                .bind(inv.id)
                .execute(&pool)
                .await
                .unwrap();
            }
            _ => unreachable!(),
        }
        let (blocked_status, blocked_body) = post_json_with_headers(
            &app,
            &format!("/poscancel/pos/invoices/{}/cancel", inv.id),
            json!({}),
            &[("authorization", bearer.as_str())],
        )
        .await;
        assert_eq!(blocked_status, StatusCode::OK);
        assert_eq!(blocked_body["code"], "InvoicePaymentAlreadyDetected");
    }

    let wallet =
        insert_wallet_origin_invoice(&pool, "poscancel", &npub, "lq1poscancelwallet", None).await;
    let foreign = insert_pos_invoice(
        &pool,
        "poscancelforeign",
        &other_npub,
        other_terminal.id,
        "lq1poscancelforeign",
        Some("Foreign"),
    )
    .await;
    for id in [wallet.id, foreign.id] {
        let (not_found_status, not_found_body) = post_json_with_headers(
            &app,
            &format!("/poscancel/pos/invoices/{id}/cancel"),
            json!({}),
            &[("authorization", bearer.as_str())],
        )
        .await;
        assert_eq!(not_found_status, StatusCode::OK);
        assert_eq!(not_found_body["code"], "InvoiceNotFound");
    }

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn invoice_status_exposes_only_public_memo_across_states() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let npub = create_test_user(&pool, "statusmemo").await;
    create_test_pos_page(&pool, "statusmemo", true).await;
    let terminal = create_claimed_terminal(&pool, "statusmemo", &npub, "statusmemo-token").await;

    let unpaid = insert_pos_invoice(
        &pool,
        "statusmemo",
        &npub,
        terminal.id,
        "lq1statusmemounpaid",
        Some("Unpaid memo"),
    )
    .await;
    let cancelled = insert_pos_invoice(
        &pool,
        "statusmemo",
        &npub,
        terminal.id,
        "lq1statusmemocancelled",
        Some("Cancelled memo"),
    )
    .await;
    pay_service::db::cancel_invoice(&pool, cancelled.id)
        .await
        .unwrap();
    let wallet_private = pay_service::db::insert_invoice(
        &pool,
        &pay_service::db::NewInvoice {
            nym_owner: Some("statusmemo"),
            npub_owner: &npub,
            origin: "wallet",
            fiat_amount_minor: None,
            fiat_currency: None,
            amount_sat: 1_000,
            rate_minor_per_btc: None,
            rate_lock_secs: 3_600,
            memo: Some("Private wallet memo"),
            terminal_id: None,
            memo_public: false,
            recipient_label: None,
            public_description: None,
            invoice_number: None,
            accept_btc: false,
            accept_ln: false,
            accept_liquid: true,
            bitcoin_address: None,
            liquid_address: Some("lq1statusmemowallet"),
            liquid_blinding_key_hex: Some("aa".repeat(32).as_str()),
            expires_in_secs: 3_600,
        },
    )
    .await
    .unwrap();

    let (_, unpaid_body) = get_path(&app, &format!("/api/v1/invoices/{}/status", unpaid.id)).await;
    assert_eq!(unpaid_body["memo"], "Unpaid memo");
    let (_, cancelled_body) =
        get_path(&app, &format!("/api/v1/invoices/{}/status", cancelled.id)).await;
    assert_eq!(cancelled_body["status"], "cancelled");
    assert_eq!(cancelled_body["memo"], "Cancelled memo");
    let (_, wallet_body) = get_path(
        &app,
        &format!("/api/v1/invoices/{}/status", wallet_private.id),
    )
    .await;
    assert!(wallet_body["memo"].is_null());

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn wallet_invoice_list_includes_terminal_fields() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let (npub, _, _, keypair) = sign_registration_with_keypair("walletposlist", TEST_DESCRIPTOR);
    pay_service::db::create_user(&pool, "walletposlist", &npub, TEST_DESCRIPTOR)
        .await
        .unwrap();
    create_test_pos_page(&pool, "walletposlist", true).await;
    let terminal =
        create_claimed_terminal(&pool, "walletposlist", &npub, "walletposlist-token").await;
    let inv = insert_pos_invoice(
        &pool,
        "walletposlist",
        &npub,
        terminal.id,
        "lq1walletposlist",
        Some("Wallet-visible memo"),
    )
    .await;
    let (sig, ts) = sign_invoice_list_with_keypair(&keypair, &npub, 1, 10, "");

    let (status, body) = get_path(
        &app,
        &format!("/api/v1/invoices?npub={npub}&timestamp={ts}&signature={sig}&page=1&pageSize=10&status="),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let item = body["invoices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == inv.id.to_string())
        .expect("terminal invoice in wallet list");
    assert_eq!(item["memo"], "Wallet-visible memo");
    assert_eq!(item["terminal_id"], terminal.id.to_string());

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn purge_expired_unclaimed_terminals_removes_only_expired_unclaimed_rows() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "pospurge").await;
    create_test_pos_page(&pool, "pospurge", true).await;

    let expired_unclaimed = pay_service::db::insert_pos_pairing(
        &pool,
        "pospurge",
        &"7".repeat(64),
        &"8".repeat(64),
        -120,
    )
    .await
    .unwrap();
    let fresh_unclaimed = pay_service::db::insert_pos_pairing(
        &pool,
        "pospurge",
        &"9".repeat(64),
        &"a".repeat(64),
        300,
    )
    .await
    .unwrap();
    let claimed = pay_service::db::claim_pos_pairing(
        &pool,
        "pospurge",
        &pay_service::db::insert_pos_pairing(
            &pool,
            "pospurge",
            &"b".repeat(64),
            &"c".repeat(64),
            300,
        )
        .await
        .unwrap()
        .pairing_code_hash
        .unwrap(),
        &npub,
        None,
        TEST_POS_DESCRIPTOR,
    )
    .await
    .unwrap()
    .unwrap();
    sqlx::query(
        "UPDATE pos_terminals SET pairing_expires_at = NOW() - INTERVAL '2 minutes' WHERE id = $1",
    )
    .bind(claimed.id)
    .execute(&pool)
    .await
    .unwrap();

    let purged = pay_service::db::purge_expired_unclaimed_terminals(&pool, 60)
        .await
        .unwrap();
    assert_eq!(purged, 1);
    assert!(
        pay_service::db::get_pos_pairing(&pool, expired_unclaimed.id, &"7".repeat(64))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        pay_service::db::get_pos_pairing(&pool, fresh_unclaimed.id, &"9".repeat(64))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        pay_service::db::get_active_terminal_by_token(&pool, "pospurge", &"b".repeat(64))
            .await
            .unwrap()
            .is_some()
    );

    cleanup_db(&pool).await;
}

async fn insert_test_invoice(
    pool: &PgPool,
    nym: &str,
    npub: &str,
    liquid_address: &str,
    expires_in_secs: i64,
) -> pay_service::db::Invoice {
    pay_service::db::insert_invoice(
        pool,
        &pay_service::db::NewInvoice {
            nym_owner: Some(nym),
            npub_owner: npub,
            origin: "checkout",
            fiat_amount_minor: None,
            fiat_currency: None,
            amount_sat: 1_000,
            rate_minor_per_btc: None,
            rate_lock_secs: expires_in_secs,
            memo: None,
            terminal_id: None,
            memo_public: false,
            recipient_label: None,
            public_description: None,
            invoice_number: None,
            accept_btc: false,
            accept_ln: false,
            accept_liquid: true,
            bitcoin_address: None,
            liquid_address: Some(liquid_address),
            liquid_blinding_key_hex: Some("11".repeat(32).as_str()),
            expires_in_secs,
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn registration_lifecycle_keeps_address_index_monotonic() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let (npub, _, _) = sign_registration("idxlife", TEST_DESCRIPTOR);

    pay_service::db::create_user(&pool, "idxlife", &npub, TEST_DESCRIPTOR)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET next_addr_idx = 4 WHERE npub = $1")
        .bind(&npub)
        .execute(&pool)
        .await
        .unwrap();

    let updated = pay_service::db::update_user_descriptor(&pool, &npub, TEST_DESCRIPTOR)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.next_addr_idx, 4);

    pay_service::db::deactivate_user(&pool, &npub)
        .await
        .unwrap()
        .unwrap();
    let reactivated =
        pay_service::db::register_user_atomic(&pool, &npub, "idxlife", TEST_DESCRIPTOR, &npub, 5)
            .await
            .unwrap();
    match reactivated {
        pay_service::db::RegisterOutcome::Reactivated(user) => {
            assert_eq!(user.next_addr_idx, 4);
        }
        _ => panic!("expected reactivation"),
    }

    let purged = pay_service::db::purge_user(&pool, &npub).await.unwrap();
    match purged {
        pay_service::db::PurgeOutcome::Purged(user) => {
            assert_eq!(user.next_addr_idx, 4);
        }
        _ => panic!("expected purge"),
    }

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn cancel_invoice_returns_final_status_on_repeated_cancel() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "cancelidem").await;
    let invoice = insert_test_invoice(
        &pool,
        "cancelidem",
        &npub,
        "lq1qqvxk052kf3qtkxmrakx50a9gc3smqad2ync54hzntjt980kfej9kkfe0247rp5h4yzmdftsahhw64uy8pzfe7cpg4fgykm7cv",
        3_600,
    )
    .await;

    let first = pay_service::db::cancel_invoice(&pool, invoice.id)
        .await
        .unwrap();
    let second = pay_service::db::cancel_invoice(&pool, invoice.id)
        .await
        .unwrap();

    assert_eq!(first, (1, "cancelled".to_string()));
    assert_eq!(second, (0, "cancelled".to_string()));

    cleanup_db(&pool).await;
}

fn liquid_direct_evidence<'a>(
    event_key: &'a str,
    amount_sat: i64,
    txid: &'a str,
    vout: i32,
    address: &'a str,
) -> pay_service::db::InvoicePaymentEvidence<'a> {
    pay_service::db::InvoicePaymentEvidence {
        rail: "liquid",
        source: "liquid_direct",
        event_key,
        amount_sat,
        txid: Some(txid),
        vout: Some(vout),
        boltz_swap_id: None,
        address: Some(address),
    }
}

fn bitcoin_direct_evidence<'a>(
    event_key: &'a str,
    amount_sat: i64,
    txid: &'a str,
    vout: i32,
    address: &'a str,
) -> pay_service::db::InvoicePaymentEvidence<'a> {
    pay_service::db::InvoicePaymentEvidence {
        rail: "bitcoin",
        source: "bitcoin_direct",
        event_key,
        amount_sat,
        txid: Some(txid),
        vout: Some(vout),
        boltz_swap_id: None,
        address: Some(address),
    }
}

fn bitcoin_direct_observation<'a>(
    event_key: &'a str,
    amount_sat: i64,
    txid: &'a str,
    vout: i32,
    address: &'a str,
    confirmations: i32,
    block_height: Option<i32>,
    last_seen_state: &'a str,
) -> pay_service::db::NewInvoicePaymentObservation<'a> {
    pay_service::db::NewInvoicePaymentObservation {
        rail: "bitcoin",
        source: "bitcoin_direct",
        event_key,
        txid,
        vout,
        address,
        amount_sat,
        confirmations,
        block_height,
        last_seen_state,
    }
}

async fn insert_test_btc_invoice(
    pool: &PgPool,
    nym: &str,
    npub: &str,
    bitcoin_address: &str,
) -> Result<pay_service::db::Invoice, sqlx::Error> {
    pay_service::db::insert_invoice(
        pool,
        &pay_service::db::NewInvoice {
            nym_owner: Some(nym),
            npub_owner: npub,
            origin: "wallet",
            fiat_amount_minor: None,
            fiat_currency: None,
            amount_sat: 1_000,
            rate_minor_per_btc: None,
            rate_lock_secs: 3_600,
            memo: None,
            terminal_id: None,
            memo_public: false,
            recipient_label: None,
            public_description: None,
            invoice_number: None,
            accept_btc: true,
            accept_ln: false,
            accept_liquid: false,
            bitcoin_address: Some(bitcoin_address),
            liquid_address: None,
            liquid_blinding_key_hex: None,
            expires_in_secs: 3_600,
        },
    )
    .await
}

#[tokio::test]
async fn invoice_insert_rejects_reused_bitcoin_address() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "btcreuse").await;
    let address = "bc1qreuseinvoiceaddress000000000000000000000000";

    let first = insert_test_btc_invoice(&pool, "btcreuse", &npub, address).await;
    assert!(first.is_ok());

    let err = insert_test_btc_invoice(&pool, "btcreuse", &npub, address)
        .await
        .unwrap_err();
    let app_error = pay_service::error::AppError::from(err);
    assert_eq!(app_error.code(), "BitcoinAddressAlreadyUsed");

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM invoices WHERE bitcoin_address = $1")
        .bind(address)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn signed_invoice_create_canonicalizes_bitcoin_address_before_reuse_check() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let (npub, _, _, keypair) = sign_registration_with_keypair("invoicecase", TEST_DESCRIPTOR);
    let expires_at_unix = auth_timestamp() as i64 + 3_600;
    let lower = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";
    let upper = "BC1QW508D6QEJXTDG4Y5R3ZARVARY0C5XW7KV8F3T4";

    let (sig_upper, ts_upper) =
        sign_invoice_create_with_keypair(&keypair, &npub, upper, expires_at_unix);
    let (status, body) = post_json(
        &app,
        "/api/v1/invoices",
        json!({
            "npub": npub,
            "amount_sat": 1000,
            "fiat_amount_minor": null,
            "fiat_currency": null,
            "public_description": null,
            "recipient_name": null,
            "invoice_number": null,
            "accept_btc": true,
            "accept_ln": false,
            "accept_liquid": false,
            "bitcoin_address": upper,
            "liquid_address": null,
            "liquid_blinding_key_hex": null,
            "expires_at_unix": expires_at_unix,
            "timestamp": ts_upper,
            "signature": sig_upper,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["invoice_id"].is_string(), "body: {body}");

    let stored: String =
        sqlx::query_scalar("SELECT bitcoin_address FROM invoices WHERE npub_owner = $1")
            .bind(&npub)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, lower);

    let (sig_lower, ts_lower) =
        sign_invoice_create_with_keypair(&keypair, &npub, lower, expires_at_unix);
    let (status, body) = post_json(
        &app,
        "/api/v1/invoices",
        json!({
            "npub": npub,
            "amount_sat": 1000,
            "fiat_amount_minor": null,
            "fiat_currency": null,
            "public_description": null,
            "recipient_name": null,
            "invoice_number": null,
            "accept_btc": true,
            "accept_ln": false,
            "accept_liquid": false,
            "bitcoin_address": lower,
            "liquid_address": null,
            "liquid_blinding_key_hex": null,
            "expires_at_unix": expires_at_unix,
            "timestamp": ts_lower,
            "signature": sig_lower,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["status"], "ERROR");
    assert_eq!(body["code"], "BitcoinAddressAlreadyUsed");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn signed_invoice_create_defaults_expiry_when_omitted() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let (npub, _, _, keypair) = sign_registration_with_keypair("invoicedefault", TEST_DESCRIPTOR);
    let address = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";

    let (sig, ts) = sign_invoice_create_without_expiry_with_keypair(&keypair, &npub, address);
    let before = auth_timestamp() as i64;
    let (status, body) = post_json(
        &app,
        "/api/v1/invoices",
        json!({
            "npub": npub,
            "amount_sat": 1000,
            "fiat_amount_minor": null,
            "fiat_currency": null,
            "public_description": null,
            "recipient_name": null,
            "invoice_number": null,
            "accept_btc": true,
            "bitcoin_address": address,
            "liquid_address": null,
            "liquid_blinding_key_hex": null,
            "timestamp": ts,
            "signature": sig,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["invoice_id"].is_string(), "body: {body}");
    let expires_at_unix: i64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM expires_at)::BIGINT FROM invoices WHERE npub_owner = $1",
    )
    .bind(&npub)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        expires_at_unix >= before + 7 * 24 * 60 * 60 - 2,
        "expires_at_unix={expires_at_unix}, before={before}"
    );
    assert!(
        expires_at_unix <= before + 7 * 24 * 60 * 60 + 2,
        "expires_at_unix={expires_at_unix}, before={before}"
    );

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn signed_invoice_list_is_auth_bound_and_npub_isolated() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let (alice_npub, _, _, alice_keypair) =
        sign_registration_with_keypair("listalice", TEST_DESCRIPTOR);
    let (bob_npub, _, _, bob_keypair) = sign_registration_with_keypair("listbob", TEST_DESCRIPTOR);
    pay_service::db::create_user(&pool, "listalice", &alice_npub, TEST_DESCRIPTOR)
        .await
        .unwrap();
    pay_service::db::create_user(&pool, "listbob", &bob_npub, TEST_DESCRIPTOR)
        .await
        .unwrap();
    let alice_invoice =
        insert_test_invoice(&pool, "listalice", &alice_npub, "lq1listalice", 3_600).await;
    let _bob_invoice = insert_test_invoice(&pool, "listbob", &bob_npub, "lq1listbob", 3_600).await;

    let (sig, timestamp) = sign_invoice_list_with_keypair(&alice_keypair, &alice_npub, 1, 10, "");
    let (status, body) = get_path(
        &app,
        &format!(
            "/api/v1/invoices?npub={alice_npub}&page=1&pageSize=10&timestamp={timestamp}&signature={sig}"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let invoices = body["invoices"].as_array().unwrap();
    assert_eq!(invoices.len(), 1, "body: {body}");
    assert_eq!(invoices[0]["id"], alice_invoice.id.to_string());
    assert_eq!(invoices[0]["nym_owner"], "listalice");

    let (forged_sig, forged_timestamp) =
        sign_invoice_list_with_keypair(&bob_keypair, &alice_npub, 1, 10, "");
    let (status, body) = get_path(
        &app,
        &format!(
            "/api/v1/invoices?npub={alice_npub}&page=1&pageSize=10&timestamp={forged_timestamp}&signature={forged_sig}"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "AuthError");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn signed_invoice_cancel_is_owner_bound_and_idempotent() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let (alice_npub, _, _, alice_keypair) =
        sign_registration_with_keypair("cancelalice", TEST_DESCRIPTOR);
    let (bob_npub, _, _, bob_keypair) =
        sign_registration_with_keypair("cancelbob", TEST_DESCRIPTOR);
    pay_service::db::create_user(&pool, "cancelalice", &alice_npub, TEST_DESCRIPTOR)
        .await
        .unwrap();
    pay_service::db::create_user(&pool, "cancelbob", &bob_npub, TEST_DESCRIPTOR)
        .await
        .unwrap();
    let invoice =
        insert_test_invoice(&pool, "cancelalice", &alice_npub, "lq1cancelalice", 3_600).await;
    let invoice_id = invoice.id.to_string();

    let (wrong_sig, wrong_timestamp) =
        sign_invoice_cancel_with_keypair(&bob_keypair, &bob_npub, "cancelbob", &invoice_id);
    let (status, body) = delete_json_path(
        &app,
        &format!("/api/v1/cancelbob/invoices/{invoice_id}"),
        json!({
            "npub": bob_npub,
            "timestamp": wrong_timestamp,
            "signature": wrong_sig,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["code"], "InvoiceNotFound");
    let still_unpaid = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still_unpaid.status, "unpaid");

    let (sig, timestamp) =
        sign_invoice_cancel_with_keypair(&alice_keypair, &alice_npub, "cancelalice", &invoice_id);
    let (status, body) = delete_json_path(
        &app,
        &format!("/api/v1/cancelalice/invoices/{invoice_id}"),
        json!({
            "npub": alice_npub,
            "timestamp": timestamp,
            "signature": sig,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["invoice_id"], invoice_id);
    assert_eq!(body["status"], "cancelled");

    let (sig, timestamp) =
        sign_invoice_cancel_with_keypair(&alice_keypair, &alice_npub, "cancelalice", &invoice_id);
    let (status, body) = delete_json_path(
        &app,
        &format!("/api/v1/cancelalice/invoices/{invoice_id}"),
        json!({
            "npub": alice_npub,
            "timestamp": timestamp,
            "signature": sig,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["status"], "cancelled");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn invoice_render_paths_preserve_linked_owner_boundary() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let npub = create_test_user(&pool, "renderowner").await;
    let invoice = insert_test_invoice(&pool, "renderowner", &npub, "lq1renderowner", 3_600).await;

    let (status, _body) = get_path(&app, &format!("/renderowner/i/{}", invoice.id)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get_path(&app, &format!("/wrongnym/i/{}", invoice.id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["code"], "InvoiceNotFound");

    let (status, _body) = get_path(&app, &format!("/invoice/{}", invoice.id)).await;
    assert_eq!(status, StatusCode::OK);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn invoice_status_and_render_share_terminal_state_after_payment() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let npub = create_test_user(&pool, "renderpaid").await;
    let invoice = insert_test_invoice(&pool, "renderpaid", &npub, "lq1renderpaid", 3_600).await;

    pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:6161616161616161616161616161616161616161616161616161616161616161:0",
            1_000,
            "6161616161616161616161616161616161616161616161616161616161616161",
            0,
            "lq1renderpaid",
        ),
        pay_service::db::InvoiceAccountingTolerances::default(),
    )
    .await
    .unwrap();

    let (status, body) = get_path(&app, &format!("/api/v1/invoices/{}/status", invoice.id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "paid");
    assert_eq!(body["remaining_amount_sat"], 0);
    assert_eq!(body["lightning_pr"], Value::Null);

    let (status, _body) = get_path(&app, &format!("/renderpaid/i/{}", invoice.id)).await;
    assert_eq!(status, StatusCode::OK);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn signed_invoice_cancel_after_paid_is_terminal_noop() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let (npub, _, _, keypair) = sign_registration_with_keypair("cancelpaid", TEST_DESCRIPTOR);
    pay_service::db::create_user(&pool, "cancelpaid", &npub, TEST_DESCRIPTOR)
        .await
        .unwrap();
    let invoice = insert_test_invoice(&pool, "cancelpaid", &npub, "lq1cancelpaid", 3_600).await;
    pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:6262626262626262626262626262626262626262626262626262626262626262:0",
            1_000,
            "6262626262626262626262626262626262626262626262626262626262626262",
            0,
            "lq1cancelpaid",
        ),
        pay_service::db::InvoiceAccountingTolerances::default(),
    )
    .await
    .unwrap();

    let invoice_id = invoice.id.to_string();
    let (sig, timestamp) =
        sign_invoice_cancel_with_keypair(&keypair, &npub, "cancelpaid", &invoice_id);
    let (status, body) = delete_json_path(
        &app,
        &format!("/api/v1/cancelpaid/invoices/{invoice_id}"),
        json!({
            "npub": npub,
            "timestamp": timestamp,
            "signature": sig,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["status"], "paid");
    let still_paid = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still_paid.status, "paid");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn invoice_insert_rejects_reused_liquid_address() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "liqreuse").await;
    let address = "lq1qqreuseinvoiceaddress000000000000000000000000";

    let _ = insert_test_invoice(&pool, "liqreuse", &npub, address, 3_600).await;

    let err = pay_service::db::insert_invoice(
        &pool,
        &pay_service::db::NewInvoice {
            nym_owner: Some("liqreuse"),
            npub_owner: &npub,
            origin: "wallet",
            fiat_amount_minor: None,
            fiat_currency: None,
            amount_sat: 1_000,
            rate_minor_per_btc: None,
            rate_lock_secs: 3_600,
            memo: None,
            terminal_id: None,
            memo_public: false,
            recipient_label: None,
            public_description: None,
            invoice_number: None,
            accept_btc: false,
            accept_ln: false,
            accept_liquid: true,
            bitcoin_address: None,
            liquid_address: Some(address),
            liquid_blinding_key_hex: Some("22".repeat(32).as_str()),
            expires_in_secs: 3_600,
        },
    )
    .await
    .unwrap_err();
    let app_error = pay_service::error::AppError::from(err);
    assert_eq!(app_error.code(), "LiquidAddressAlreadyUsed");

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM invoices WHERE liquid_address = $1")
        .bind(address)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn checkout_liquid_allocator_skips_addresses_already_assigned_to_invoices() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "allocskip").await;
    let reused = "lq1allocskipreused000000000000000000000000";
    let fresh = "lq1allocskipfresh0000000000000000000000000";

    let _ = insert_test_invoice(&pool, "allocskip", &npub, reused, 3_600).await;

    let allocated = pay_service::db::allocate_next_liquid_for_active_nym(
        &pool,
        "allocskip",
        |_descriptor, idx| match idx {
            0 => Ok(reused.to_string()),
            1 => Ok(fresh.to_string()),
            other => Err(sqlx::Error::Protocol(format!("unexpected index {other}"))),
        },
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(allocated, (fresh.to_string(), 1));
    let next_idx: i32 = sqlx::query_scalar("SELECT next_addr_idx FROM users WHERE nym = $1")
        .bind("allocskip")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(next_idx, 2);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn liquid_outpoint_reservation_reuses_original_index_after_cursor_advances() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let _npub = create_test_user(&pool, "liquidlastunused").await;
    let outpoint_a = "00".repeat(32);
    let outpoint_b = "11".repeat(32);
    let pubkey_a = "02".repeat(33);
    let pubkey_b = "03".repeat(33);

    let first = pay_service::db::allocate_outpoint_address(
        &pool,
        "liquidlastunused",
        &outpoint_a,
        &pubkey_a,
    )
    .await
    .unwrap();
    assert_eq!(first, 0);

    sqlx::query("UPDATE users SET next_addr_idx = 7 WHERE nym = $1")
        .bind("liquidlastunused")
        .execute(&pool)
        .await
        .unwrap();

    let repeated = pay_service::db::allocate_outpoint_address(
        &pool,
        "liquidlastunused",
        &outpoint_a,
        &pubkey_a,
    )
    .await
    .unwrap();
    assert_eq!(repeated, 0);

    let different_outpoint = pay_service::db::allocate_outpoint_address(
        &pool,
        "liquidlastunused",
        &outpoint_b,
        &pubkey_b,
    )
    .await
    .unwrap();
    assert_eq!(different_outpoint, 7);

    let reservations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM outpoint_addresses WHERE nym = $1")
            .bind("liquidlastunused")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reservations, 2);

    cleanup_db(&pool).await;
}

// --- Invoice lifecycle / watcher database coverage ---

#[tokio::test]
async fn invoice_expiry_gc_marks_only_active_past_deadline_rows() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "invoicegc").await;

    let expired_unpaid =
        insert_test_invoice(&pool, "invoicegc", &npub, "lq1expiredunpaid", -10).await;
    let expired_in_progress =
        insert_test_invoice(&pool, "invoicegc", &npub, "lq1expiredinprogress", -10).await;
    let expired_paid = insert_test_invoice(&pool, "invoicegc", &npub, "lq1expiredpaid", -10).await;
    let fresh_unpaid = insert_test_invoice(&pool, "invoicegc", &npub, "lq1freshunpaid", 60).await;

    pay_service::db::mark_invoice_in_progress(&pool, expired_in_progress.id)
        .await
        .unwrap();
    pay_service::db::record_invoice_payment(
        &pool,
        expired_paid.id,
        liquid_direct_evidence(
            "liquid_direct:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:0",
            1_000,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            0,
            "lq1expiredpaid",
        ),
        pay_service::db::InvoiceAccountingTolerances::default(),
    )
    .await
    .unwrap();

    let expired_count = pay_service::db::expire_invoices_past_deadline(&pool)
        .await
        .unwrap();
    assert_eq!(expired_count, 2);

    let expired_unpaid = pay_service::db::get_invoice_by_id(&pool, expired_unpaid.id)
        .await
        .unwrap()
        .unwrap();
    let expired_in_progress = pay_service::db::get_invoice_by_id(&pool, expired_in_progress.id)
        .await
        .unwrap()
        .unwrap();
    let expired_paid = pay_service::db::get_invoice_by_id(&pool, expired_paid.id)
        .await
        .unwrap()
        .unwrap();
    let fresh_unpaid = pay_service::db::get_invoice_by_id(&pool, fresh_unpaid.id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(expired_unpaid.status, "expired");
    assert_eq!(expired_in_progress.status, "expired");
    assert_eq!(expired_paid.status, "paid");
    assert_eq!(fresh_unpaid.status, "unpaid");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn invoice_payment_events_track_partial_completion_and_overpay() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "eventacct").await;
    let invoice = insert_test_invoice(&pool, "eventacct", &npub, "lq1eventacct", 60).await;
    let tolerances = pay_service::db::InvoiceAccountingTolerances {
        btc_sat: 300,
        liquid_sat: 60,
        lightning_sat: 1,
    };

    let rows = pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb:0",
            400,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            0,
            "lq1eventacct",
        ),
        tolerances,
    )
    .await
    .unwrap();
    assert_eq!(rows, 1);

    let partial = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(partial.status, "partially_paid");
    assert_eq!(partial.settlement_status, "none");
    assert_eq!(partial.paid_via.as_deref(), Some("liquid"));
    assert_eq!(partial.paid_amount_sat, Some(400));
    assert!(partial.paid_at_unix.is_none());

    let duplicate_rows = pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb:0",
            400,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            0,
            "lq1eventacct",
        ),
        tolerances,
    )
    .await
    .unwrap();
    assert_eq!(duplicate_rows, 0);

    let rows = pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        bitcoin_direct_evidence(
            "bitcoin_direct:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc:0",
            590,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            0,
            "bc1qeventacct",
        ),
        tolerances,
    )
    .await
    .unwrap();
    assert_eq!(rows, 1);

    let paid = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paid.status, "paid");
    assert_eq!(paid.settlement_status, "settled");
    assert_eq!(paid.paid_via.as_deref(), Some("mixed"));
    assert_eq!(paid.paid_amount_sat, Some(990));
    assert!(paid.paid_at_unix.is_some());

    let rows = pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        bitcoin_direct_evidence(
            "bitcoin_direct:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd:1",
            20,
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            1,
            "bc1qeventacct",
        ),
        tolerances,
    )
    .await
    .unwrap();
    assert_eq!(rows, 1);

    let overpaid = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(overpaid.status, "overpaid");
    assert_eq!(overpaid.settlement_status, "settled");
    assert_eq!(overpaid.paid_amount_sat, Some(1_010));

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn boltz_liquid_payout_does_not_double_count_lightning_invoice_payment() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "boltzpayout").await;
    let invoice = insert_test_invoice(&pool, "boltzpayout", &npub, "lq1boltzpayout", 60).await;
    let tolerances = pay_service::db::InvoiceAccountingTolerances::default();
    let claim_txid = "8843a083f1db2d9f857f18025fbf9bf1e3b256fb0c06bebae207fa7a01218e88";

    let rows = pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:8843a083f1db2d9f857f18025fbf9bf1e3b256fb0c06bebae207fa7a01218e88:0",
            951,
            claim_txid,
            0,
            "lq1boltzpayout",
        ),
        tolerances,
    )
    .await
    .unwrap();
    assert_eq!(rows, 1);

    invoice::flip_invoice_on_lightning_settlement(
        &pool,
        Some(invoice.id),
        1_000,
        "boltz-payout-race",
        claim_txid,
        tolerances,
    )
    .await;

    let paid = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paid.status, "paid");
    assert_eq!(paid.paid_via.as_deref(), Some("lightning"));
    assert_eq!(paid.paid_amount_sat, Some(1_000));

    let events: Vec<(String, i64)> = sqlx::query_as(
        "SELECT source, amount_sat FROM invoice_payment_events \
         WHERE invoice_id = $1 ORDER BY source",
    )
    .bind(invoice.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(events, vec![("lightning_boltz_reverse".to_string(), 1_000)]);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn liquid_scanner_ignores_known_boltz_settlement_txid() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "boltzknown").await;
    let invoice = insert_test_invoice(&pool, "boltzknown", &npub, "lq1boltzknown", 60).await;
    let tolerances = pay_service::db::InvoiceAccountingTolerances::default();
    let claim_txid = "9843a083f1db2d9f857f18025fbf9bf1e3b256fb0c06bebae207fa7a01218e89";

    invoice::flip_invoice_on_lightning_settlement(
        &pool,
        Some(invoice.id),
        1_000,
        "boltz-known-first",
        claim_txid,
        tolerances,
    )
    .await;

    let rows = pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:9843a083f1db2d9f857f18025fbf9bf1e3b256fb0c06bebae207fa7a01218e89:0",
            951,
            claim_txid,
            0,
            "lq1boltzknown",
        ),
        tolerances,
    )
    .await
    .unwrap();
    assert_eq!(rows, 0);

    let paid = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paid.status, "paid");
    assert_eq!(paid.paid_via.as_deref(), Some("lightning"));
    assert_eq!(paid.paid_amount_sat, Some(1_000));

    let event_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM invoice_payment_events WHERE invoice_id = $1")
            .bind(invoice.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(event_count.0, 1);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn boltz_settlement_does_not_prune_direct_bitcoin_payment_events() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "boltzbtcdirect").await;
    let invoice = insert_test_btc_invoice(&pool, "boltzbtcdirect", &npub, "bc1qboltzbtcdirect")
        .await
        .unwrap();
    let tolerances = pay_service::db::InvoiceAccountingTolerances::default();
    let txid = "a843a083f1db2d9f857f18025fbf9bf1e3b256fb0c06bebae207fa7a01218e8a";

    let rows = pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        bitcoin_direct_evidence(
            "bitcoin_direct:a843a083f1db2d9f857f18025fbf9bf1e3b256fb0c06bebae207fa7a01218e8a:0",
            100,
            txid,
            0,
            "bc1qboltzbtcdirect",
        ),
        tolerances,
    )
    .await
    .unwrap();
    assert_eq!(rows, 1);

    invoice::flip_invoice_on_lightning_settlement(
        &pool,
        Some(invoice.id),
        1_000,
        "boltz-does-not-prune-btc",
        txid,
        tolerances,
    )
    .await;

    let overpaid = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(overpaid.status, "overpaid");
    assert_eq!(overpaid.paid_via.as_deref(), Some("mixed"));
    assert_eq!(overpaid.paid_amount_sat, Some(1_100));

    let events: Vec<(String, i64)> = sqlx::query_as(
        "SELECT source, amount_sat FROM invoice_payment_events \
         WHERE invoice_id = $1 ORDER BY source",
    )
    .bind(invoice.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        events,
        vec![
            ("bitcoin_direct".to_string(), 100),
            ("lightning_boltz_reverse".to_string(), 1_000),
        ]
    );

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn bitcoin_payment_observations_do_not_count_as_paid() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "btcobserve").await;
    let invoice = insert_test_btc_invoice(&pool, "btcobserve", &npub, "bc1qbtcobserve")
        .await
        .unwrap();

    let rows = pay_service::db::upsert_invoice_payment_observation(
        &pool,
        invoice.id,
        bitcoin_direct_observation(
            "bitcoin_direct:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee:0",
            1_000,
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            0,
            "bc1qbtcobserve",
            0,
            None,
            "seen_unconfirmed",
        ),
    )
    .await
    .unwrap();
    assert_eq!(rows, 1);

    let invoice = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(invoice.status, "unpaid");
    assert_eq!(invoice.settlement_status, "none");
    assert_eq!(invoice.paid_amount_sat, None);
    assert_eq!(invoice.paid_via, None);

    let observations = pay_service::db::list_invoice_payment_observations(&pool, invoice.id, 10)
        .await
        .unwrap();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].last_seen_state, "seen_unconfirmed");
    assert_eq!(observations[0].confirmations, 0);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn bitcoin_payment_observation_upsert_updates_confirmation_state() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "btcconfirm").await;
    let invoice = insert_test_btc_invoice(&pool, "btcconfirm", &npub, "bc1qbtcconfirm")
        .await
        .unwrap();
    let event_key =
        "bitcoin_direct:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff:1";
    let txid = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    pay_service::db::upsert_invoice_payment_observation(
        &pool,
        invoice.id,
        bitcoin_direct_observation(
            event_key,
            1_000,
            txid,
            1,
            "bc1qbtcconfirm",
            0,
            None,
            "seen_unconfirmed",
        ),
    )
    .await
    .unwrap();
    pay_service::db::upsert_invoice_payment_observation(
        &pool,
        invoice.id,
        bitcoin_direct_observation(
            event_key,
            1_000,
            txid,
            1,
            "bc1qbtcconfirm",
            2,
            Some(800_000),
            "awaiting_confirmations",
        ),
    )
    .await
    .unwrap();

    let observations = pay_service::db::list_invoice_payment_observations(&pool, invoice.id, 10)
        .await
        .unwrap();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].confirmations, 2);
    assert_eq!(observations[0].block_height, Some(800_000));
    assert_eq!(observations[0].last_seen_state, "awaiting_confirmations");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn missing_bitcoin_observation_is_marked_not_seen_without_accounting_change() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "btcmissing").await;
    let invoice = insert_test_btc_invoice(&pool, "btcmissing", &npub, "bc1qbtcmissing")
        .await
        .unwrap();

    pay_service::db::upsert_invoice_payment_observation(
        &pool,
        invoice.id,
        bitcoin_direct_observation(
            "bitcoin_direct:1111111111111111111111111111111111111111111111111111111111111111:0",
            500,
            "1111111111111111111111111111111111111111111111111111111111111111",
            0,
            "bc1qbtcmissing",
            0,
            None,
            "seen_unconfirmed",
        ),
    )
    .await
    .unwrap();

    let rows =
        pay_service::db::mark_missing_bitcoin_payment_observations_not_seen(&pool, invoice.id, &[])
            .await
            .unwrap();
    assert_eq!(rows, 1);

    let observations = pay_service::db::list_invoice_payment_observations(&pool, invoice.id, 10)
        .await
        .unwrap();
    assert_eq!(observations[0].last_seen_state, "not_seen");
    let invoice = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(invoice.paid_amount_sat, None);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn invoice_status_exposes_bitcoin_direct_observations() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "btcstatus").await;
    let invoice = insert_test_btc_invoice(&pool, "btcstatus", &npub, "bc1qbtcstatusx")
        .await
        .unwrap();
    pay_service::db::upsert_invoice_payment_observation(
        &pool,
        invoice.id,
        bitcoin_direct_observation(
            "bitcoin_direct:2222222222222222222222222222222222222222222222222222222222222222:0",
            750,
            "2222222222222222222222222222222222222222222222222222222222222222",
            0,
            "bc1qbtcstatusx",
            0,
            None,
            "seen_unconfirmed",
        ),
    )
    .await
    .unwrap();

    let app = test_app(test_state(pool.clone()));
    let (status, body) = get_path(&app, &format!("/api/v1/invoices/{}/status", invoice.id)).await;
    assert_eq!(status, StatusCode::OK);
    let observations = body["bitcoin_direct_observations"].as_array().unwrap();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0]["source"], "bitcoin_direct");
    assert_eq!(observations[0]["rail"], "bitcoin");
    assert_eq!(
        observations[0]["txid"],
        "2222222222222222222222222222222222222222222222222222222222222222"
    );
    assert_eq!(observations[0]["vout"], 0);
    assert_eq!(observations[0]["amount_sat"], 750);
    assert_eq!(observations[0]["state"], "seen_unconfirmed");
    assert_eq!(body["status"], "unpaid");
    assert_eq!(body["paid_amount_sat"], Value::Null);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn stale_checkout_partial_terminalizes_to_underpaid() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "checkoutstale").await;
    let invoice =
        insert_test_invoice(&pool, "checkoutstale", &npub, "lq1checkoutstale", 3_600).await;

    pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:1212121212121212121212121212121212121212121212121212121212121212:0",
            400,
            "1212121212121212121212121212121212121212121212121212121212121212",
            0,
            "lq1checkoutstale",
        ),
        pay_service::db::InvoiceAccountingTolerances::default(),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE invoice_payment_events \
         SET created_at = NOW() - INTERVAL '20 minutes' \
         WHERE invoice_id = $1",
    )
    .bind(invoice.id)
    .execute(&pool)
    .await
    .unwrap();

    let rows = pay_service::db::terminalize_stale_checkout_partial_invoice(&pool, invoice.id, 900)
        .await
        .unwrap();
    assert_eq!(rows, 1);

    let underpaid = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(underpaid.status, "underpaid");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn stale_wallet_partial_stays_payable() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "walletpartial").await;
    let blinding_key = "11".repeat(32);
    let invoice = pay_service::db::insert_invoice(
        &pool,
        &pay_service::db::NewInvoice {
            nym_owner: Some("walletpartial"),
            npub_owner: &npub,
            origin: "wallet",
            fiat_amount_minor: None,
            fiat_currency: None,
            amount_sat: 1_000,
            rate_minor_per_btc: None,
            rate_lock_secs: 3_600,
            memo: None,
            terminal_id: None,
            memo_public: false,
            recipient_label: None,
            public_description: None,
            invoice_number: None,
            accept_btc: false,
            accept_ln: false,
            accept_liquid: true,
            bitcoin_address: None,
            liquid_address: Some("lq1walletpartial"),
            liquid_blinding_key_hex: Some(&blinding_key),
            expires_in_secs: 3_600,
        },
    )
    .await
    .unwrap();

    pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:1313131313131313131313131313131313131313131313131313131313131313:0",
            400,
            "1313131313131313131313131313131313131313131313131313131313131313",
            0,
            "lq1walletpartial",
        ),
        pay_service::db::InvoiceAccountingTolerances::default(),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE invoice_payment_events \
         SET created_at = NOW() - INTERVAL '20 minutes' \
         WHERE invoice_id = $1",
    )
    .bind(invoice.id)
    .execute(&pool)
    .await
    .unwrap();

    let rows = pay_service::db::terminalize_stale_checkout_partial_invoice(&pool, invoice.id, 900)
        .await
        .unwrap();
    assert_eq!(rows, 0);

    let partial = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(partial.status, "partially_paid");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn checkout_underpaid_liquid_address_remains_watchable_and_recoverable() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "underwatch").await;
    let invoice = insert_test_invoice(&pool, "underwatch", &npub, "lq1underwatch", 3_600).await;

    pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:1414141414141414141414141414141414141414141414141414141414141414:0",
            400,
            "1414141414141414141414141414141414141414141414141414141414141414",
            0,
            "lq1underwatch",
        ),
        pay_service::db::InvoiceAccountingTolerances::default(),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE invoice_payment_events \
         SET created_at = NOW() - INTERVAL '20 minutes' \
         WHERE invoice_id = $1",
    )
    .bind(invoice.id)
    .execute(&pool)
    .await
    .unwrap();
    pay_service::db::terminalize_stale_checkout_partial_invoice(&pool, invoice.id, 900)
        .await
        .unwrap();

    let candidates = pay_service::db::list_unpaid_invoices_with_liquid_address(&pool)
        .await
        .unwrap();
    assert!(candidates
        .iter()
        .any(|(candidate_id, address, _, _)| *candidate_id == invoice.id
            && address == "lq1underwatch"));

    let rows = pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:1515151515151515151515151515151515151515151515151515151515151515:1",
            600,
            "1515151515151515151515151515151515151515151515151515151515151515",
            1,
            "lq1underwatch",
        ),
        pay_service::db::InvoiceAccountingTolerances::default(),
    )
    .await
    .unwrap();
    assert_eq!(rows, 1);

    let paid = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paid.status, "paid");
    assert_eq!(paid.paid_amount_sat, Some(1_000));

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn checkout_underpaid_insufficient_topup_stays_underpaid() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "undertopup").await;
    let invoice = insert_test_invoice(&pool, "undertopup", &npub, "lq1undertopup", 3_600).await;

    pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:1616161616161616161616161616161616161616161616161616161616161616:0",
            300,
            "1616161616161616161616161616161616161616161616161616161616161616",
            0,
            "lq1undertopup",
        ),
        pay_service::db::InvoiceAccountingTolerances::default(),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE invoice_payment_events \
         SET created_at = NOW() - INTERVAL '20 minutes' \
         WHERE invoice_id = $1",
    )
    .bind(invoice.id)
    .execute(&pool)
    .await
    .unwrap();
    pay_service::db::terminalize_stale_checkout_partial_invoice(&pool, invoice.id, 900)
        .await
        .unwrap();

    let rows = pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:1717171717171717171717171717171717171717171717171717171717171717:1",
            200,
            "1717171717171717171717171717171717171717171717171717171717171717",
            1,
            "lq1undertopup",
        ),
        pay_service::db::InvoiceAccountingTolerances::default(),
    )
    .await
    .unwrap();
    assert_eq!(rows, 1);

    let underpaid = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(underpaid.status, "underpaid");
    assert_eq!(underpaid.paid_amount_sat, Some(500));

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn invoice_status_terminalizes_stale_checkout_partial_before_response() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let npub = create_test_user(&pool, "statusunder").await;
    let invoice = insert_test_invoice(&pool, "statusunder", &npub, "lq1statusunder", 3_600).await;

    pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:1818181818181818181818181818181818181818181818181818181818181818:0",
            400,
            "1818181818181818181818181818181818181818181818181818181818181818",
            0,
            "lq1statusunder",
        ),
        pay_service::db::InvoiceAccountingTolerances::default(),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE invoice_payment_events \
         SET created_at = NOW() - INTERVAL '20 minutes' \
         WHERE invoice_id = $1",
    )
    .bind(invoice.id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = get_path(&app, &format!("/api/v1/invoices/{}/status", invoice.id)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "underpaid");
    assert_eq!(body["paid_amount_sat"], 400);
    assert_eq!(body["remaining_amount_sat"], 600);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn invoice_status_surfaces_partial_payment_remaining_amount() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));
    let npub = create_test_user(&pool, "partialstatus").await;
    let invoice = insert_test_invoice(&pool, "partialstatus", &npub, "lq1partialstatus", 60).await;

    pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:abababababababababababababababababababababababababababababababab:0",
            400,
            "abababababababababababababababababababababababababababababababab",
            0,
            "lq1partialstatus",
        ),
        pay_service::db::InvoiceAccountingTolerances::default(),
    )
    .await
    .unwrap();

    let (status, body) = get_path(&app, &format!("/api/v1/invoices/{}/status", invoice.id)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "partially_paid");
    assert_eq!(body["paid_amount_sat"], 400);
    assert_eq!(body["remaining_amount_sat"], 600);
    assert_eq!(body["settlement_status"], "none");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn invoice_payment_events_store_direct_and_boltz_evidence() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "eventevidence").await;
    let invoice = insert_test_invoice(&pool, "eventevidence", &npub, "lq1eventevidence", 60).await;
    let tolerances = pay_service::db::InvoiceAccountingTolerances::default();

    let _ = pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:1111111111111111111111111111111111111111111111111111111111111111:2",
            100,
            "1111111111111111111111111111111111111111111111111111111111111111",
            2,
            "lq1eventevidence",
        ),
        tolerances,
    )
    .await
    .unwrap();
    let direct: (String, String, String, i32, Option<String>, String, i64) = sqlx::query_as(
        "SELECT rail, source, txid, vout, boltz_swap_id, address, amount_sat \
         FROM invoice_payment_events WHERE event_key = $1",
    )
    .bind("liquid_direct:1111111111111111111111111111111111111111111111111111111111111111:2")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(direct.0, "liquid");
    assert_eq!(direct.1, "liquid_direct");
    assert_eq!(
        direct.2,
        "1111111111111111111111111111111111111111111111111111111111111111"
    );
    assert_eq!(direct.3, 2);
    assert!(direct.4.is_none());
    assert_eq!(direct.5, "lq1eventevidence");
    assert_eq!(direct.6, 100);

    invoice::flip_invoice_on_lightning_settlement(
        &pool,
        Some(invoice.id),
        100,
        "boltz-reverse-evidence",
        "2222222222222222222222222222222222222222222222222222222222222222",
        tolerances,
    )
    .await;
    let boltz: (String, String, String, Option<i32>, String, Option<String>) = sqlx::query_as(
        "SELECT rail, source, txid, vout, boltz_swap_id, address \
         FROM invoice_payment_events WHERE event_key = $1",
    )
    .bind("lightning_boltz_reverse:boltz-reverse-evidence")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(boltz.0, "lightning");
    assert_eq!(boltz.1, "lightning_boltz_reverse");
    assert_eq!(
        boltz.2,
        "2222222222222222222222222222222222222222222222222222222222222222"
    );
    assert!(boltz.3.is_none());
    assert_eq!(boltz.4, "boltz-reverse-evidence");
    assert!(boltz.5.is_none());

    invoice::flip_invoice_on_bitcoin_boltz_settlement(
        &pool,
        Some(invoice.id),
        100,
        "boltz-chain-evidence",
        "3333333333333333333333333333333333333333333333333333333333333333",
        tolerances,
    )
    .await;
    let chain: (String, String, String, Option<i32>, String, Option<String>) = sqlx::query_as(
        "SELECT rail, source, txid, vout, boltz_swap_id, address \
         FROM invoice_payment_events WHERE event_key = $1",
    )
    .bind("bitcoin_boltz_chain:boltz-chain-evidence")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(chain.0, "bitcoin");
    assert_eq!(chain.1, "bitcoin_boltz_chain");
    assert_eq!(
        chain.2,
        "3333333333333333333333333333333333333333333333333333333333333333"
    );
    assert!(chain.3.is_none());
    assert_eq!(chain.4, "boltz-chain-evidence");
    assert!(chain.5.is_none());

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn invoice_payment_event_constraints_reject_invalid_evidence() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "eventconstraints").await;
    let invoice =
        insert_test_invoice(&pool, "eventconstraints", &npub, "lq1eventconstraints", 60).await;

    let wrong_rail = sqlx::query(
        "INSERT INTO invoice_payment_events \
            (invoice_id, rail, source, event_key, amount_sat, txid, vout, address) \
         VALUES ($1, 'bitcoin', 'liquid_direct', $2, 1, $3, 0, 'lq1eventconstraints')",
    )
    .bind(invoice.id)
    .bind("liquid_direct:4444444444444444444444444444444444444444444444444444444444444444:0")
    .bind("4444444444444444444444444444444444444444444444444444444444444444")
    .execute(&pool)
    .await;
    assert!(wrong_rail.is_err());

    let missing_direct_address = sqlx::query(
        "INSERT INTO invoice_payment_events \
            (invoice_id, rail, source, event_key, amount_sat, txid, vout) \
         VALUES ($1, 'liquid', 'liquid_direct', $2, 1, $3, 0)",
    )
    .bind(invoice.id)
    .bind("liquid_direct:5555555555555555555555555555555555555555555555555555555555555555:0")
    .bind("5555555555555555555555555555555555555555555555555555555555555555")
    .execute(&pool)
    .await;
    assert!(missing_direct_address.is_err());

    let missing_boltz_txid = sqlx::query(
        "INSERT INTO invoice_payment_events \
            (invoice_id, rail, source, event_key, amount_sat, boltz_swap_id) \
         VALUES ($1, 'lightning', 'lightning_boltz_reverse', $2, 1, 'swap-without-txid')",
    )
    .bind(invoice.id)
    .bind("lightning_boltz_reverse:swap-without-txid")
    .execute(&pool)
    .await;
    assert!(missing_boltz_txid.is_err());

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn invoice_only_lightning_swap_does_not_require_nym() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "invoiceonly").await;
    let invoice = insert_test_invoice(&pool, "invoiceonly", &npub, "lq1invoiceonly", 60).await;

    pay_service::db::record_swap(
        &pool,
        &pay_service::db::NewSwapRecord {
            nym: None,
            boltz_swap_id: "invoiceonly-swap",
            address: None,
            address_index: None,
            amount_sat: 1_000,
            invoice: "lnbc-invoiceonly",
            preimage_hex: "aa".repeat(32).as_str(),
            claim_key_hex: "bb".repeat(32).as_str(),
            boltz_response_json: "{}",
            invoice_id: Some(invoice.id),
        },
    )
    .await
    .unwrap();

    let pr = pay_service::db::latest_lightning_pr_for_invoice(&pool, invoice.id)
        .await
        .unwrap();
    assert_eq!(
        pr.as_ref().map(|(bolt11, _)| bolt11.as_str()),
        Some("lnbc-invoiceonly")
    );

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn settlement_status_tracks_pending_and_claim_incidents() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "settlementstate").await;
    let invoice =
        insert_test_invoice(&pool, "settlementstate", &npub, "lq1settlementstate", 60).await;

    pay_service::db::mark_invoice_in_progress(&pool, invoice.id)
        .await
        .unwrap();
    let pending = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.status, "in_progress");
    assert_eq!(pending.settlement_status, "pending");

    pay_service::db::mark_invoice_settlement_status(&pool, Some(invoice.id), "claim_stuck")
        .await
        .unwrap();
    let stuck = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stuck.settlement_status, "claim_stuck");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn expired_partial_payment_becomes_underpaid() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "eventunderpaid").await;
    let invoice =
        insert_test_invoice(&pool, "eventunderpaid", &npub, "lq1eventunderpaid", -10).await;

    pay_service::db::record_invoice_payment(
        &pool,
        invoice.id,
        liquid_direct_evidence(
            "liquid_direct:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee:0",
            400,
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            0,
            "lq1eventunderpaid",
        ),
        pay_service::db::InvoiceAccountingTolerances::default(),
    )
    .await
    .unwrap();

    let underpaid = pay_service::db::get_invoice_by_id(&pool, invoice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(underpaid.status, "underpaid");
    assert_eq!(underpaid.paid_amount_sat, Some(400));
    assert_eq!(underpaid.paid_via.as_deref(), Some("liquid"));
    assert!(underpaid.paid_at_unix.is_none());

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn liquid_address_watcher_scan_excludes_expired_invoice_rows() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "liquidscan").await;

    let expired = insert_test_invoice(&pool, "liquidscan", &npub, "lq1scanexpired", -10).await;
    let fresh = insert_test_invoice(&pool, "liquidscan", &npub, "lq1scanfresh", 60).await;

    let rows = pay_service::db::list_unpaid_invoices_with_liquid_address(&pool)
        .await
        .unwrap();
    let invoice_ids: std::collections::HashSet<_> =
        rows.into_iter().map(|(id, _, _, _)| id).collect();

    assert!(!invoice_ids.contains(&expired.id));
    assert!(invoice_ids.contains(&fresh.id));

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn latest_lightning_pr_for_invoice_uses_newest_swap_row() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "latestpr").await;
    let invoice = insert_test_invoice(&pool, "latestpr", &npub, "lq1latestpr", 60).await;

    pay_service::db::record_swap(
        &pool,
        &pay_service::db::NewSwapRecord {
            nym: Some("latestpr"),
            boltz_swap_id: "latestpr-old",
            address: Some("lq1latestold"),
            address_index: Some(0),
            amount_sat: 1_000,
            invoice: "lnbc-old",
            preimage_hex: "aa".repeat(32).as_str(),
            claim_key_hex: "bb".repeat(32).as_str(),
            boltz_response_json: "{}",
            invoice_id: Some(invoice.id),
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE swap_records SET created_at = NOW() - INTERVAL '1 minute' WHERE boltz_swap_id = 'latestpr-old'")
        .execute(&pool)
        .await
        .unwrap();

    pay_service::db::record_swap(
        &pool,
        &pay_service::db::NewSwapRecord {
            nym: Some("latestpr"),
            boltz_swap_id: "latestpr-new",
            address: Some("lq1latestnew"),
            address_index: Some(1),
            amount_sat: 1_000,
            invoice: "lnbc-new",
            preimage_hex: "cc".repeat(32).as_str(),
            claim_key_hex: "dd".repeat(32).as_str(),
            boltz_response_json: "{}",
            invoice_id: Some(invoice.id),
        },
    )
    .await
    .unwrap();

    let pr = pay_service::db::latest_lightning_pr_for_invoice(&pool, invoice.id)
        .await
        .unwrap();
    assert_eq!(
        pr.as_ref().map(|(bolt11, _)| bolt11.as_str()),
        Some("lnbc-new")
    );

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn chain_swap_records_are_invoice_scoped_and_retrievable() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "chainswaprec").await;
    let invoice = insert_test_invoice(&pool, "chainswaprec", &npub, "lq1chainswaprec", 60).await;

    let row = pay_service::db::record_chain_swap(
        &pool,
        &pay_service::db::NewChainSwapRecord {
            invoice_id: invoice.id,
            nym: Some("chainswaprec"),
            boltz_swap_id: "chain-swap-rec-1",
            lockup_address: "bc1qchainswaplockup",
            lockup_bip21: Some("bitcoin:bc1qchainswaplockup?amount=0.00001000"),
            user_lock_amount_sat: 1_000,
            server_lock_amount_sat: 990,
            preimage_hex: "11".repeat(32).as_str(),
            claim_key_hex: "22".repeat(32).as_str(),
            refund_key_hex: "33".repeat(32).as_str(),
            boltz_response_json: "{\"id\":\"chain-swap-rec-1\"}",
        },
    )
    .await
    .unwrap();
    assert_eq!(row.status, "pending");
    assert_eq!(row.from_chain, "BTC");
    assert_eq!(row.to_chain, "L-BTC");
    assert_eq!(row.claim_tx_hex, None);
    assert_eq!(row.claim_attempts, 0);
    assert_eq!(row.last_claim_error, None);

    let latest = pay_service::db::latest_payable_chain_swap_for_invoice(&pool, invoice.id, 1_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.boltz_swap_id, "chain-swap-rec-1");
    assert_eq!(
        latest.lockup_bip21.as_deref(),
        Some("bitcoin:bc1qchainswaplockup?amount=0.00001000")
    );
    let wrong_amount =
        pay_service::db::latest_payable_chain_swap_for_invoice(&pool, invoice.id, 999)
            .await
            .unwrap();
    assert!(
        wrong_amount.is_none(),
        "chain-swap offers must match the invoice's current remaining amount"
    );
    pay_service::db::update_chain_swap_status(
        &pool,
        row.id,
        pay_service::db::ChainSwapStatus::Expired,
        None,
    )
    .await
    .unwrap();
    let stale = pay_service::db::latest_payable_chain_swap_for_invoice(&pool, invoice.id, 1_000)
        .await
        .unwrap();
    assert!(
        stale.is_none(),
        "expired chain swaps must not be exposed as payable offers"
    );

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn ready_to_claim_chain_swaps_includes_retry_rows_with_claim_txid() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "chainretry").await;
    let invoice = insert_test_invoice(&pool, "chainretry", &npub, "lq1chainretry", 60).await;

    let row = pay_service::db::record_chain_swap(
        &pool,
        &pay_service::db::NewChainSwapRecord {
            invoice_id: invoice.id,
            nym: Some("chainretry"),
            boltz_swap_id: "chainretry-swap",
            lockup_address: "bc1qchainretrylockup",
            lockup_bip21: None,
            user_lock_amount_sat: 1_000,
            server_lock_amount_sat: 990,
            preimage_hex: "11".repeat(32).as_str(),
            claim_key_hex: "22".repeat(32).as_str(),
            refund_key_hex: "33".repeat(32).as_str(),
            boltz_response_json: "{\"id\":\"chainretry-swap\"}",
        },
    )
    .await
    .unwrap();
    pay_service::db::update_chain_swap_status(
        &pool,
        row.id,
        pay_service::db::ChainSwapStatus::ServerLockConfirmed,
        None,
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE chain_swap_records \
         SET status = 'claiming', \
             claim_txid = 'chain-retry-claim-txid', \
             claim_tx_hex = 'deadbeef', \
             next_claim_attempt_at = NOW() - INTERVAL '1 second' \
         WHERE boltz_swap_id = 'chainretry-swap'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let ready = pay_service::db::get_ready_to_claim_chain_swaps(&pool)
        .await
        .unwrap();
    let retry = ready
        .iter()
        .find(|row| row.boltz_swap_id == "chainretry-swap")
        .expect("claiming chain swap with persisted claim tx must be retryable");

    assert_eq!(retry.claim_txid.as_deref(), Some("chain-retry-claim-txid"));
    assert_eq!(retry.claim_tx_hex.as_deref(), Some("deadbeef"));

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn chain_swap_claim_failure_transitions_to_stuck_at_budget() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "chainfail").await;
    let invoice = insert_test_invoice(&pool, "chainfail", &npub, "lq1chainfail", 60).await;

    let row = pay_service::db::record_chain_swap(
        &pool,
        &pay_service::db::NewChainSwapRecord {
            invoice_id: invoice.id,
            nym: Some("chainfail"),
            boltz_swap_id: "chainfail-swap",
            lockup_address: "bc1qchainfaillockup",
            lockup_bip21: None,
            user_lock_amount_sat: 1_000,
            server_lock_amount_sat: 990,
            preimage_hex: "11".repeat(32).as_str(),
            claim_key_hex: "22".repeat(32).as_str(),
            refund_key_hex: "33".repeat(32).as_str(),
            boltz_response_json: "{\"id\":\"chainfail-swap\"}",
        },
    )
    .await
    .unwrap();
    pay_service::db::update_chain_swap_status(
        &pool,
        row.id,
        pay_service::db::ChainSwapStatus::ServerLockConfirmed,
        None,
    )
    .await
    .unwrap();

    let outcome = pay_service::db::record_chain_swap_claim_failure(
        &pool,
        row.id,
        "synthetic claim failure",
        1,
    )
    .await
    .unwrap();
    assert_eq!(outcome, pay_service::db::ClaimFailureOutcome::Stuck);

    let row = pay_service::db::get_chain_swap_by_boltz_id(&pool, "chainfail-swap")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "claim_stuck");
    assert_eq!(row.claim_attempts, 1);
    assert_eq!(
        row.last_claim_error.as_deref(),
        Some("synthetic claim failure")
    );

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn ready_to_claim_swaps_includes_retry_rows_with_claim_txid() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let npub = create_test_user(&pool, "claimretry").await;
    let invoice = insert_test_invoice(&pool, "claimretry", &npub, "lq1claimretry", 60).await;

    pay_service::db::record_swap(
        &pool,
        &pay_service::db::NewSwapRecord {
            nym: Some("claimretry"),
            boltz_swap_id: "claimretry-swap",
            address: Some("lq1claimretryaddr"),
            address_index: Some(0),
            amount_sat: 1_000,
            invoice: "lnbc-claimretry",
            preimage_hex: "aa".repeat(32).as_str(),
            claim_key_hex: "bb".repeat(32).as_str(),
            boltz_response_json: "{}",
            invoice_id: Some(invoice.id),
        },
    )
    .await
    .unwrap();

    sqlx::query(
        "UPDATE swap_records \
         SET status = 'claiming', \
             claim_txid = 'retry-claim-txid', \
             claim_tx_hex = 'deadbeef', \
             next_claim_attempt_at = NOW() - INTERVAL '1 second' \
         WHERE boltz_swap_id = 'claimretry-swap'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let ready = pay_service::db::get_ready_to_claim_swaps(&pool)
        .await
        .unwrap();
    let retry = ready
        .iter()
        .find(|row| row.boltz_swap_id == "claimretry-swap")
        .expect("claiming swap with persisted claim tx must be retryable");

    assert_eq!(retry.claim_txid.as_deref(), Some("retry-claim-txid"));
    assert_eq!(retry.claim_tx_hex.as_deref(), Some("deadbeef"));

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn purge_with_no_swaps_scrubs_descriptor_and_keeps_nym_reserved() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (npub, sig, timestamp, keypair) =
        sign_registration_with_keypair("purger1", TEST_DESCRIPTOR);
    post_json(
        &app,
        "/register",
        json!({
            "nym": "purger1", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": sig, "timestamp": timestamp,
        }),
    )
    .await;

    let (purge_sig, purge_timestamp) = sign_purge_with_keypair(&keypair, &npub, "purger1");
    let (status, _) = delete_request(
        &app,
        json!({
            "npub": npub, "nym": "purger1", "signature": purge_sig, "purge": true, "timestamp": purge_timestamp,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // LNURL no longer resolves
    let (_, body) = get_path(&app, "/.well-known/lnurlp/purger1").await;
    assert_eq!(body["status"], "ERROR");

    // Row survives with scrubbed descriptor and is_active=false
    let row: (bool, String) =
        sqlx::query_as("SELECT is_active, ct_descriptor FROM users WHERE nym = $1")
            .bind("purger1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!row.0);
    assert_eq!(row.1, "");

    // Another npub cannot claim the reserved nym
    let (npub2, sig2, timestamp2) = sign_registration("purger1", TEST_DESCRIPTOR);
    let (_, body) = post_json(
        &app,
        "/register",
        json!({
            "nym": "purger1", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub2, "verification_npub": npub2, "signature": sig2, "timestamp": timestamp2,
        }),
    )
    .await;
    assert_eq!(body["status"], "ERROR");

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn purge_blocked_when_pending_swap_exists() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (npub, sig, timestamp, keypair) =
        sign_registration_with_keypair("purger2", TEST_DESCRIPTOR);
    post_json(
        &app,
        "/register",
        json!({
            "nym": "purger2", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": sig, "timestamp": timestamp,
        }),
    )
    .await;
    insert_swap(&pool, "purger2", "pending", 0).await;
    insert_swap(&pool, "purger2", "lockup_confirmed", 1).await;

    let (purge_sig, purge_timestamp) = sign_purge_with_keypair(&keypair, &npub, "purger2");
    let (_, body) = delete_request(
        &app,
        json!({
            "npub": npub, "nym": "purger2", "signature": purge_sig, "purge": true, "timestamp": purge_timestamp,
        }),
    )
    .await;
    assert_eq!(body["code"], "PurgeBlocked");
    assert!(body["reason"].as_str().unwrap().contains("2"));

    // User still active, swaps untouched
    let active: bool = sqlx::query_scalar("SELECT is_active FROM users WHERE nym = 'purger2'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(active);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM swap_records WHERE nym = 'purger2'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn purge_drops_only_terminal_swap_history() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (npub, sig, timestamp, keypair) =
        sign_registration_with_keypair("purger3", TEST_DESCRIPTOR);
    post_json(
        &app,
        "/register",
        json!({
            "nym": "purger3", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": sig, "timestamp": timestamp,
        }),
    )
    .await;
    insert_swap(&pool, "purger3", "claimed", 0).await;
    insert_swap(&pool, "purger3", "expired", 1).await;

    let (purge_sig, purge_timestamp) = sign_purge_with_keypair(&keypair, &npub, "purger3");
    let (status, _) = delete_request(
        &app,
        json!({
            "npub": npub, "nym": "purger3", "signature": purge_sig, "purge": true, "timestamp": purge_timestamp,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM swap_records WHERE nym = 'purger3'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn delete_signature_does_not_authorize_purge() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (npub, sig, timestamp, keypair) =
        sign_registration_with_keypair("purger4", TEST_DESCRIPTOR);
    post_json(
        &app,
        "/register",
        json!({
            "nym": "purger4", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": sig, "timestamp": timestamp,
        }),
    )
    .await;
    insert_swap(&pool, "purger4", "claimed", 0).await;

    // Sign the soft-delete challenge but try to use it for purge
    let (delete_sig, delete_timestamp) = sign_delete_with_keypair(&keypair, &npub, "purger4");
    let (status, _) = delete_request(
        &app,
        json!({
            "npub": npub, "nym": "purger4", "signature": delete_sig, "purge": true, "timestamp": delete_timestamp,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // User still active, swap_records intact
    let active: bool = sqlx::query_scalar("SELECT is_active FROM users WHERE nym = 'purger4'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(active);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM swap_records WHERE nym = 'purger4'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    cleanup_db(&pool).await;
}

#[tokio::test]
async fn purge_then_owner_reregisters_same_nym() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    let (npub, sig, timestamp, keypair) =
        sign_registration_with_keypair("purger5", TEST_DESCRIPTOR);
    post_json(
        &app,
        "/register",
        json!({
            "nym": "purger5", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": sig, "timestamp": timestamp,
        }),
    )
    .await;

    let (purge_sig, purge_timestamp) = sign_purge_with_keypair(&keypair, &npub, "purger5");
    delete_request(
        &app,
        json!({
            "npub": npub, "nym": "purger5", "signature": purge_sig, "purge": true, "timestamp": purge_timestamp,
        }),
    )
    .await;

    // Same owner re-registers same nym
    let (re_sig, re_timestamp) =
        sign_register_with_keypair(&keypair, &npub, "purger5", TEST_DESCRIPTOR);
    let (status, body) = post_json(
        &app,
        "/register",
        json!({
            "nym": "purger5", "ct_descriptor": TEST_DESCRIPTOR, "npub": npub, "verification_npub": npub, "signature": re_sig, "timestamp": re_timestamp,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["nym"], "purger5");

    cleanup_db(&pool).await;
}

#[test]
fn schnorr_sign_verify_roundtrip() {
    // This tests the exact same flow the mobile app uses:
    // 1. Generate a keypair
    // 2. Sign SHA256(message) with schnorr
    // 3. Verify with our auth::verify_signature

    use secp256k1::{Keypair, Secp256k1};
    use sha2::{Digest, Sha256};

    let secp = Secp256k1::new();
    let keypair = Keypair::new(&secp, &mut secp256k1::rand::thread_rng());
    let (xonly, _parity) = keypair.x_only_public_key();
    let npub_hex = xonly.to_string();

    let message = b"tester1ct(slip77(...),elwpkh(...))";
    let digest = Sha256::digest(message);
    let msg = secp256k1::Message::from_digest(*digest.as_ref());
    let sig = secp.sign_schnorr(&msg, &keypair);
    let sig_hex = sig.to_string();

    println!("npub: {npub_hex}");
    println!("sig:  {sig_hex}");
    println!("sig len: {}", sig_hex.len());

    // Verify using our auth module
    let result = pay_service::auth::verify_signature(&npub_hex, message, &sig_hex);
    assert!(
        result.is_ok(),
        "Signature verification failed: {:?}",
        result
    );
}

/// Two concurrent registers from the same npub, when only one slot remains
/// under the lifetime cap, must result in exactly one Created and one
/// non-success response — never two Createds (which would overshoot the cap)
/// and never InternalError (the bug pre-advisory-lock).
#[tokio::test]
async fn register_concurrent_does_not_exceed_cap() {
    let pool = test_pool().await;
    cleanup_db(&pool).await;
    let app = test_app(test_state(pool.clone()));

    // One keypair → one npub for all calls.
    let (npub_hex, _, _, kp) = sign_registration_with_keypair("filler-0", TEST_DESCRIPTOR);

    // Burn 2 of 3 lifetime slots (`LimitsConfig::default()` cap = 3) by
    // creating + deactivating filler rows. Goes through the atomic flow
    // sequentially so the partial unique on active-npub isn't violated.
    pay_service::db::register_user_atomic(
        &pool,
        &npub_hex,
        "filler-0",
        TEST_DESCRIPTOR,
        &npub_hex,
        3,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE users SET is_active = FALSE WHERE nym = 'filler-0'")
        .execute(&pool)
        .await
        .unwrap();
    pay_service::db::register_user_atomic(
        &pool,
        &npub_hex,
        "filler-1",
        TEST_DESCRIPTOR,
        &npub_hex,
        3,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE users SET is_active = FALSE WHERE nym = 'filler-1'")
        .execute(&pool)
        .await
        .unwrap();

    // Two concurrent register calls — only one slot remains. Without the
    // advisory lock, both would pass `used < cap` (read=2) and both would
    // INSERT, leaving 4 lifetime rows. With the lock, the loser sees either
    // the active nym created by the winner or the exhausted lifetime cap.
    let (sig_a, timestamp_a) =
        sign_register_with_keypair(&kp, &npub_hex, "conc-a", TEST_DESCRIPTOR);
    let (sig_b, timestamp_b) =
        sign_register_with_keypair(&kp, &npub_hex, "conc-b", TEST_DESCRIPTOR);

    let req_a = post_json(
        &app,
        "/register",
        json!({
            "nym": "conc-a",
            "ct_descriptor": TEST_DESCRIPTOR,
            "npub": npub_hex,
            "verification_npub": npub_hex,
            "signature": sig_a,
            "timestamp": timestamp_a,
        }),
    );
    let req_b = post_json(
        &app,
        "/register",
        json!({
            "nym": "conc-b",
            "ct_descriptor": TEST_DESCRIPTOR,
            "npub": npub_hex,
            "verification_npub": npub_hex,
            "signature": sig_b,
            "timestamp": timestamp_b,
        }),
    );
    let ((status_a, body_a), (status_b, body_b)) = tokio::join!(req_a, req_b);

    let success_count =
        (status_a == StatusCode::CREATED) as u32 + (status_b == StatusCode::CREATED) as u32;
    let guarded_reject_count = matches!(
        body_a["code"].as_str(),
        Some("NymQuotaExceeded" | "KeyAlreadyRegistered")
    ) as u32
        + matches!(
            body_b["code"].as_str(),
            Some("NymQuotaExceeded" | "KeyAlreadyRegistered")
        ) as u32;

    assert_eq!(
        success_count, 1,
        "exactly one register should succeed; got a=({status_a:?},{body_a}) b=({status_b:?},{body_b})"
    );
    assert_eq!(
        guarded_reject_count, 1,
        "the other should be rejected by the atomic registration guard; got a={body_a} b={body_b}"
    );
    // The bug we're guarding against: race-loser returning a generic
    // InternalError because the cap check happened outside the atomic tx.
    let codes = [body_a["code"].as_str(), body_b["code"].as_str()];
    assert!(
        !codes.contains(&Some("InternalError")),
        "must not return InternalError under contention; got {codes:?}"
    );

    // DB invariant: exactly cap rows under this npub.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE npub = $1")
        .bind(&npub_hex)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 3, "lifetime cap must hold under contention");

    cleanup_db(&pool).await;
}
