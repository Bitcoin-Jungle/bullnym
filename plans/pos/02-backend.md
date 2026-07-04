# Backend Changes

Minimal by design. The Rust server already has everything needed. Changes are additive only.

## Migration: pos_mode column

One new column on `donation_pages`:

```sql
-- migrations/0032_pos_mode.sql
ALTER TABLE donation_pages
  ADD COLUMN pos_mode BOOLEAN NOT NULL DEFAULT FALSE;
```

Default `FALSE` means all existing nyms remain in donation mode. POS mode is opt-in,
set by Bull Wallet via the existing signed `PUT /donation-page/:nym` endpoint (add
`pos_mode` to the `UpsertDonationPage` struct and the upsert SQL).

## donation_render.rs: serve PWA instead of Askama template

Replace the Askama `DonationPageTpl` render with static file serving:

```rust
// donation_render.rs (simplified diff)

// Before: renders store_amount.html via Askama
// After: serves the appropriate PWA shell

async fn render_live(nym: &str, page: &DonationPage, state: &AppState) -> Response {
    let shell = if page.pos_mode {
        state.pwa_shells.pos.clone()          // bytes of dist/pos/index.html
    } else {
        state.pwa_shells.donation.clone()     // bytes of dist/donation/index.html
    };

    // Inject config JSON into the shell before sending
    let config = serde_json::json!({
        "nym": nym,
        "mode": if page.pos_mode { "pos" } else { "donation" },
        "currency": page.display_currency,
        "header": page.header,
        "description": page.description,
    });
    let injected = inject_config(&shell, &config);

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        injected,
    ).into_response()
}

fn inject_config(html: &str, config: &serde_json::Value) -> String {
    // Replace placeholder comment in index.html with the config script block
    html.replace(
        "<!-- BULLNYM_CONFIG -->",
        &format!(
            r#"<script id="bullnym-config" type="application/json">{}</script>"#,
            config
        ),
    )
}
```

`AppState` gains a `pwa_shells: PwaShells` field loaded at startup from `dist/`.

## Static asset serving

Add to `build_router`:

```rust
router = router.nest_service(
    "/pwa-assets",
    ServeDir::new("pwa/dist/assets").precompressed_gzip(),
);
```

The PWA's Vite build outputs hashed asset filenames (JS/CSS), so this path is
cache-forever friendly.

## Invoice status endpoint: ensure public access

`GET /api/v1/invoices/:id/status` — verify this route has no Schnorr auth middleware
in the current router. If it does, ensure the PWA can poll it unauthenticated.
Looking at the current router, it's in the `api/v1` block alongside signed endpoints.
May need to be split out to its own route group without the auth layer.

**Check:** grep the router for where `status` is registered vs where the auth
middleware is applied. If it's behind auth, move it to the public block.

## UpsertDonationPage: add pos_mode field

```rust
// src/db/donation_pages.rs
pub struct UpsertDonationPage<'a> {
    // ... existing fields ...
    pub pos_mode: bool,   // new
}
```

Update the SQL in `upsert_donation_page` to include `pos_mode` in the INSERT and
UPDATE clauses.

Update `PUT /donation-page/:nym` handler and the bullpay-la-v2 signing field list
(add `"pos_mode"` as a signable field).

## POS terminal capability

Server-authoritative POS terminals are implemented. Pairing, bearer-token
terminal auth, memo-bearing terminal invoice creation, terminal invoice history,
terminal cancellation, wallet terminal listing, and wallet revocation are covered
by the contract in `docs/features/pos-terminals.md`.

## No other backend changes

The entire payment flow (LNURL, Boltz swap creation, webhook, claimer, chain_watcher,
reconciler) is untouched. The PWA is purely a new frontend for existing API endpoints.
