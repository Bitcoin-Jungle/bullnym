# POS Terminals

POS terminals are browser-based cash registers paired to a wallet-owned payment
page. The terminal can create checkout invoices and read the nym-scoped terminal
history with a bearer token. Wallet ownership and terminal management remain
signed with `bullpay-la-v2`.

## Table of Contents

- [Signing Contract](#signing-contract)
- [Shared Test Vectors](#shared-test-vectors)
- [Pairing and Token Model](#pairing-and-token-model)
- [Terminal Runtime](#terminal-runtime)
- [Fund Separation](#fund-separation)
- [Endpoints](#endpoints)
- [Memo and Receipt Semantics](#memo-and-receipt-semantics)
- [Revocation](#revocation)
- [Rate Limits](#rate-limits)
- [Accepted Risk](#accepted-risk)

## Signing Contract

All wallet-signed POS actions use the same `bullpay-la-v2` byte builder as the
rest of Bullnym. The message is raw UTF-8 bytes joined with NUL bytes:

```text
bullpay-la-v2\0<action>\0<npub_hex>\0<nym_or_empty>\0<payload_field>\0...<timestamp-as-decimal-string>
```

Rules:

- `npub_hex` is the 64-character lowercase hex x-only public key.
- `timestamp` is decimal ASCII with no NUL before it except the separator after
  the last previous field.
- Empty fields are encoded by writing nothing between separators.
- Payload fields are exactly the fields after `nym_or_empty`; do not add names,
  JSON, lengths, or extra trailing separators.

Action layouts:

| Action | `nym_or_empty` | Payload fields |
|---|---|---|
| `pos-pair` | merchant nym | `code`, then `label-or-empty`, then `pos_ct_descriptor` |
| `pos-terminal-list` | empty string | none |
| `pos-terminal-revoke` | empty string | `terminal_id` as lowercase hyphenated UUID string |

Expanded forms:

```text
pos-pair:
bullpay-la-v2\0pos-pair\0<npub_hex>\0<nym>\0<code>\0<label-or-empty>\0<pos_ct_descriptor>\0<timestamp>

pos-terminal-list:
bullpay-la-v2\0pos-terminal-list\0<npub_hex>\0\0<timestamp>

pos-terminal-revoke:
bullpay-la-v2\0pos-terminal-revoke\0<npub_hex>\0\0<terminal_id>\0<timestamp>
```

## Shared Test Vectors

These are the cross-repo contract checks from `src/pos/tests.rs`. The wallet
must reproduce the full expected message hex byte-for-byte.

Common inputs:

```text
npub = 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
nym = testnym
code = ABCDEFGH
label = Front Counter
pos_ct_descriptor = ct(slip77(9c8e4f05c7711a98c838be228bcb84924d4570ca53f35fa1c793e58841d47023),elwpkh([73c5da0a/84h/1776h/0h]xpub6CRFzUgHFDaiDAQFNX7VeV9JNPDRabq6NYSpzVZ8zW8ANUCiDdenkb1gBoEZuXNZb3wPc1SVcDXgD2ww5UBtTb8s8ArAbTkoRQ8qn34KgcY/<0;1>/*))#y8jljyxl
timestamp = 1750000000
terminal_id = 550e8400-e29b-41d4-a716-446655440000
```

`pos-pair`:

```text
action = pos-pair
nym_or_empty = testnym
payload_fields = [ABCDEFGH, Front Counter, ct(slip77(9c8e4f05c7711a98c838be228bcb84924d4570ca53f35fa1c793e58841d47023),elwpkh([73c5da0a/84h/1776h/0h]xpub6CRFzUgHFDaiDAQFNX7VeV9JNPDRabq6NYSpzVZ8zW8ANUCiDdenkb1gBoEZuXNZb3wPc1SVcDXgD2ww5UBtTb8s8ArAbTkoRQ8qn34KgcY/<0;1>/*))#y8jljyxl]
expected_message_hex = 62756c6c7061792d6c612d763200706f732d70616972003031323334353637383961626364656630313233343536373839616263646566303132333435363738396162636465663031323334353637383961626364656600746573746e796d0041424344454647480046726f6e7420436f756e74657200637428736c697037372839633865346630356337373131613938633833386265323238626362383439323464343537306361353366333566613163373933653538383431643437303233292c656c77706b68285b37336335646130612f3834682f31373736682f30685d78707562364352467a55674846446169444151464e5837566556394a4e504452616271364e5953707a565a387a5738414e5543694464656e6b623167426f455a75584e5a6233775063315356634458674432777735554274546238733841724162546b6f525138716e33344b6763592f3c303b313e2f2a29292379386a6c6a79786c0031373530303030303030
```

`pos-terminal-list`:

```text
action = pos-terminal-list
nym_or_empty = 
payload_fields = []
expected_message_hex = 62756c6c7061792d6c612d763200706f732d7465726d696e616c2d6c6973740030313233343536373839616263646566303132333435363738396162636465663031323334353637383961626364656630313233343536373839616263646566000031373530303030303030
```

`pos-terminal-revoke`:

```text
action = pos-terminal-revoke
nym_or_empty = 
payload_fields = [550e8400-e29b-41d4-a716-446655440000]
expected_message_hex = 62756c6c7061792d6c612d763200706f732d7465726d696e616c2d7265766f6b650030313233343536373839616263646566303132333435363738396162636465663031323334353637383961626364656630313233343536373839616263646566000035353065383430302d653239622d343164342d613731362d3434363635353434303030300031373530303030303030
```

## Pairing and Token Model

The terminal generates a 32-byte random token locally before creating a pairing.
It sends only `token_hash`, the lowercase hex SHA-256 digest of the raw token.
The server stores only token hashes. After pairing is approved, terminal API
requests authenticate with:

```http
Authorization: Bearer <raw-client-generated-token>
```

The server hashes the bearer token and looks up an active, claimed, non-revoked
terminal for the path nym.

Pairing codes:

- 8 characters.
- Alphabet: `ABCDEFGHJKLMNPQRSTUVWXYZ23456789`.
- No `0`, `O`, `1`, or `I`.
- Stored server-side as lowercase hex SHA-256 of the code.
- Expire after about 5 minutes.
- Single-use: claim clears `pairing_code_hash`.
- Failed claims are brute-force rate limited by source IP.

Deep link:

```text
bullbitcoin://bullnym/pos-pair?pairing_id=...&code=...&nym=...
```

Same-device flow: the terminal shows the pairing code and deep link/QR. If the
wallet is on the same device, opening the link switches to the wallet; after the
wallet signs and claims the pairing, the browser terminal keeps polling with
`pairing_id` and `token_hash` until it receives `approved`.

## Terminal Runtime

Once paired, the terminal is server-authoritative:

- The POS app is served at `GET /:nym/pos`; its manifest is
  `GET /:nym/pos/manifest.webmanifest` with `start_url` set to `/:nym/pos`.
- POS is available for any **active nym** — a configured payment page is not
  required. When no page row exists, the shell renders from a synthesized
  disabled placeholder, and the first `pos-pair` claim materializes that
  placeholder row (`enabled = false`, no page descriptor) to store the POS
  descriptor. An archived page blocks POS everywhere. The `enabled` flag only
  controls the public donation surface at `GET /:nym` and anonymous donation
  invoices at `POST /:nym/invoice` — a disabled page with an active POS
  profile keeps 404ing publicly.
- A POS-only merchant uses `enabled = false` with one or more paired terminals.
- It creates invoices through `POST /:nym/pos/invoice`.
- It polls payment state with `GET /api/v1/invoices/:id/status`.
- It reads register history from `GET /:nym/pos/invoices`.
- It cancels only terminal-attributed invoices for the same nym.
- Any `401 AuthError` from a terminal endpoint means the bearer token is missing,
  invalid, or revoked. The terminal should clear the token and return to pairing.

## Fund Separation

Each nym has three independent settlement wallets:

| Surface | Descriptor column | Wallet seed index |
|---|---|---|
| Lightning Address | `users.ct_descriptor` | 101 |
| Donation page | `donation_pages.ct_descriptor` | 102 |
| POS terminal | `donation_pages.pos_ct_descriptor` | 103 |

The POS descriptor is provisioned by the wallet-signed `pos-pair` claim. The
server verifies the signature over the raw transmitted descriptor string, then
validates that descriptor. If `donation_pages.pos_ct_descriptor` is unset, the
claim stores it while claiming the terminal (materializing the disabled
placeholder page row first when none exists). If it is already set, future
claims must send the byte-identical descriptor or the server rejects the claim
with `PosDescriptorMismatch` (HTTP 409); the stored descriptor is never
overwritten.

Terminal invoices allocate Liquid addresses from `pos_ct_descriptor` and advance
`pos_next_addr_idx`. Donation invoices continue to allocate from
`ct_descriptor` and `next_addr_idx`. A terminal invoice for a page with no POS
descriptor fails with `PosDescriptorRequired`; it never falls back to the
donation descriptor.

## Endpoints

Errors use the standard Bullnym envelope unless noted:

```json
{
  "status": "ERROR",
  "code": "InvalidAmount",
  "reason": "message"
}
```

Most application errors use HTTP 200 with this envelope. `AuthError` uses HTTP
401, `PosDescriptorMismatch` uses HTTP 409, `PosPairingClaimRateLimited` uses
HTTP 429, and `ServiceUnavailable` uses HTTP 503.

### Create Pairing

`POST /:nym/pos/pairings`

Auth: none. Requires an active nym; a payment page is not required, but an
archived page blocks pairing.

Request:

```json
{
  "token_hash": "64 lowercase hex chars"
}
```

Response:

```json
{
  "pairing_id": "uuid",
  "code": "ABCDEFGH",
  "expires_at_unix": 1750000000
}
```

Errors: `InvalidAmount` for malformed `token_hash`,
`DonationPageNotFound` when the nym is unknown/inactive or its page is
archived (code kept stable for deployed terminals),
`RateLimitedSender` when pairing creation is too frequent.

### Poll Pairing

`GET /:nym/pos/pairings/:id?token_hash=...`

Auth: possession of the client token hash created for this pairing.

Response while pending or expired:

```json
{
  "status": "pending"
}
```

```json
{
  "status": "expired"
}
```

Response after wallet approval:

```json
{
  "status": "approved",
  "terminal_id": "uuid"
}
```

Errors: `DonationPageNotFound` for missing, wrong-nym, or wrong-token pairing;
`RateLimitedSender` when polling is too frequent.

### Claim Pairing

`POST /api/v1/pos/pairings/claim`

Auth: wallet Schnorr signature over the `pos-pair` la-v2 message.

Request:

```json
{
  "npub": "64 lowercase hex chars",
  "nym": "merchant-nym",
  "code": "ABCDEFGH",
  "label": "Front Counter",
  "pos_ct_descriptor": "ct(slip77(...),elwpkh(...))#checksum",
  "timestamp": 1750000000,
  "signature": "schnorr signature hex"
}
```

`label` is optional. For signing, absent `label` is the empty string.
`pos_ct_descriptor` is required and non-empty.

Response:

```json
{
  "terminal_id": "uuid",
  "nym": "merchant-nym",
  "label": "Front Counter",
  "claimed_at_unix": 1750000000
}
```

Errors: `AuthError` for signature/timestamp failures,
`PosDescriptorMismatch` (HTTP 409) for a descriptor that differs from the
already-provisioned POS wallet, `DonationPageNotFound` for an invalid,
expired, already claimed, or wrong-nym code (or an archived page),
`InvalidAmount` for invalid label, `InvalidDescriptor` for an empty or
invalid POS descriptor, and `PosPairingClaimRateLimited` after too many
failed claims.

### Create Terminal Invoice

`POST /:nym/pos/invoice`

Auth: terminal bearer token.

Request:

```json
{
  "amount_sat": 1000,
  "fiat_amount_minor": null,
  "fiat_currency": null,
  "memo": "Two coffees"
}
```

Use either `amount_sat` or `fiat_amount_minor` plus `fiat_currency`, not both.
`memo` is optional.

Response:

```json
{
  "invoice_id": "uuid",
  "lightning_pr": "bolt11",
  "liquid_address": "liquid address",
  "bitcoin_chain_address": "bitcoin address or null",
  "bitcoin_chain_bip21": "bitcoin bip21 or null",
  "expires_at_unix": 1750000000
}
```

Errors: `AuthError`, `InvalidAmount`, `DonationPageNotFound`,
`RateLimitedSender`, `ServiceUnavailable`, `BoltzError`.

Donation-page `enabled = false` does not block terminal invoice creation.
Archived pages still reject terminal invoices.

### List Terminal Invoices

`GET /:nym/pos/invoices?page=1&pageSize=50&status=paid`

Auth: terminal bearer token.

Query:

```text
page: integer, required, 1..1000
pageSize: integer, required, clamped to max 100
status: optional; one of unpaid, in_progress, partially_paid, paid, underpaid, overpaid, expired, cancelled
```

Response:

```json
{
  "invoices": [
    {
      "id": "uuid",
      "status": "paid",
      "pricing_mode": "fiat",
      "settlement_status": "settled",
      "amount_sat": 1000,
      "remaining_amount_sat": 0,
      "fiat_amount_minor": 850000,
      "fiat_currency": "CRC",
      "memo": "Two coffees",
      "terminal_id": "uuid",
      "accept_btc": false,
      "accept_ln": true,
      "accept_liquid": true,
      "bitcoin_address": null,
      "liquid_address": "liquid address",
      "created_at_unix": 1750000000,
      "expires_at_unix": 1750003600,
      "paid_via": "ln",
      "paid_at_unix": 1750000100,
      "paid_amount_sat": 1000
    }
  ],
  "page": 1,
  "pageSize": 50,
  "has_more": false
}
```

Only terminal-attributed invoices for the path nym are returned, newest first.

Errors: `AuthError`, `InvalidAmount`.

### Cancel Terminal Invoice

`POST /:nym/pos/invoices/:id/cancel`

Auth: terminal bearer token. Body is ignored.

Response:

```json
{
  "invoice_id": "uuid",
  "status": "cancelled"
}
```

Only terminal-attributed invoices for the path nym can be cancelled. Wallet
invoices and foreign-nym invoices are hidden as `InvoiceNotFound`.

Errors: `AuthError`, `InvoiceNotFound`, `InvoicePaymentAlreadyDetected`.

### List Wallet Terminals

`GET /api/v1/pos/terminals?npub=...&timestamp=...&signature=...`

Auth: wallet Schnorr signature over the `pos-terminal-list` la-v2 message.

Response:

```json
{
  "terminals": [
    {
      "id": "uuid",
      "nym": "merchant-nym",
      "label": "Front Counter",
      "claimed_at_unix": 1750000000,
      "last_seen_at_unix": 1750000100,
      "revoked_at_unix": null,
      "created_at_unix": 1749999900
    }
  ]
}
```

Errors: `AuthError`.

### Revoke Terminal

`POST /api/v1/pos/terminals/:id/revoke`

Auth: wallet Schnorr signature over the `pos-terminal-revoke` la-v2 message.

Request:

```json
{
  "npub": "64 lowercase hex chars",
  "timestamp": 1750000000,
  "signature": "schnorr signature hex"
}
```

Response:

```json
{
  "terminal_id": "uuid",
  "revoked": true
}
```

Errors: `AuthError`, `TerminalNotFound`.

### Invoice Status

`GET /api/v1/invoices/:id/status`

Auth: none. The terminal uses this public status endpoint for payment and
receipt state.

Response:

```json
{
  "status": "paid",
  "memo": "Two coffees",
  "pricing_mode": "fiat",
  "settlement_status": "settled",
  "amount_sat": 1000,
  "fiat_amount_minor": 850000,
  "fiat_currency": "CRC",
  "remaining_amount_sat": 0,
  "payment_tolerance_sat": 1,
  "rate_minor_per_btc": 850000000000,
  "rate_locks_until_unix": 1750000300,
  "expires_at_unix": 1750003600,
  "paid_via": "ln",
  "paid_at_unix": 1750000100,
  "paid_amount_sat": 1000,
  "lightning_pr": null,
  "liquid_address": "liquid address",
  "bitcoin_address": null,
  "bitcoin_direct_observations": [],
  "bitcoin_chain_address": null,
  "bitcoin_chain_bip21": null,
  "accept_btc": false,
  "accept_ln": true,
  "accept_liquid": true
}
```

Errors: `InvoiceNotFound`, `InvalidAmount`, `RateLimitedSender`.

## Memo and Receipt Semantics

Terminal-created invoices store `memo` and set `memo_public = true` when the
memo is non-empty. Empty memo values are normalized away. Memos must not contain
control characters and are capped at 280 Unicode scalar values.

Public receipt/status visibility:

- `GET /api/v1/invoices/:id/status` returns `memo` only when `memo_public` is
  true.
- Linked payment pages render the memo only when `memo_public` is true.
- Terminal history includes the memo for terminal-attributed rows.
- Wallet-only invoice fields such as `recipient_label`, `public_description`,
  and `invoice_number` are never set on terminal invoices.

BOLT11 description rule: when `memo_public` is true and `memo` is ASCII and at
most 100 bytes, the Lightning offer uses the memo as the Boltz `description`.
Otherwise it falls back to the public invoice URL/URL hash rule. The Boltz
request sends either `description` or `description_hash`, never both.

## Revocation

Revoking a terminal sets `revoked_at`. Existing invoices remain in history and
continue through payment/settlement accounting, but the terminal token no longer
authenticates. A terminal that receives `401 AuthError` should clear its local
token and re-enter the pairing flow.

## Rate Limits

Default expectations:

| Endpoint | Bucket | Default |
|---|---:|---:|
| `POST /:nym/pos/pairings` | source IP | 5/hour |
| `GET /:nym/pos/pairings/:id` | source IP | 120/5 min |
| `POST /api/v1/pos/pairings/claim` failed claims | source IP | 20/hour |
| `POST /:nym/pos/invoice` | terminal id | 30/5 min |
| `GET /api/v1/invoices/:id/status` | source IP | 60/min |

Wallet terminal list and revoke endpoints are signed and do not currently have
POS-specific rate buckets beyond the shared infrastructure.

## Accepted Risk

The browser terminal stores the raw bearer token in localStorage. This is an
accepted v1 risk because the terminal is a cashier device and the token cannot
move funds or change wallet settlement configuration.

Mitigations:

- Token scope is nym-bound.
- The server stores only SHA-256 token hashes.
- Wallet owners can revoke terminals.
- Revoked or invalid tokens return `401 AuthError`, causing the terminal to
  re-pair.
- Terminal APIs create and manage receivables only; they cannot spend funds.
