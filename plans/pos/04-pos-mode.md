# POS PWA

The cashier-initiated flow. Merchant has a tablet at the counter. Cashier enters an
amount, customer scans QR or taps Bolt Card, payment completes, receipt prints.

Server update 2026-07-04: POS is no longer selected by a `donation_pages.pos_mode`
flag. The POS app has its own route at `/<nym>/pos`, with manifest
`/<nym>/pos/manifest.webmanifest`. Any existing, non-archived payment page can
host POS terminals. The page `enabled` flag now controls only the public
donation surface (`/<nym>` and anonymous donation invoices), so POS-only
merchants use `enabled = false` with paired terminals.

## Routes

```
/#/          → keypad (main cashier screen)
/#/pay/:id   → payment screen for invoice :id
/#/receipt/:id → receipt for paid invoice :id
/#/history   → transaction history sheet
/#/settings  → settings (PIN-gated)
```

## Keypad screen (`/#/`)

```
┌─────────────────────────────────┐
│  Seguras Butcher        [≡ Menu]│
│                                 │
│  ₡ 0                            │
│                                 │
│  [1] [2] [3]                    │
│  [4] [5] [6]                    │
│  [7] [8] [9]                    │
│  [00] [0] [⌫]                   │
│                                 │
│  [Add note]                     │
│                                 │
│  [Charge]                       │
│                                 │
│  Recent Transactions ▴          │
└─────────────────────────────────┘
```

- Amount display: large, clear, formatted in display currency
- Keypad: `1–9`, `00`, `0`, `⌫`. No decimal entry for CRC/whole-unit currencies;
  decimal enabled for USD/CAD/EUR (two decimal places)
- "Add note": optional memo sent to the server with the terminal invoice
- "Charge": disabled until amount > 0 and rate is fresh (< 5 min)
- Recent Transactions: pull-up sheet handle (see history section)
- [≡ Menu]: PIN-gated settings

## Charging flow

1. Cashier presses "Charge"
2. PWA calls `POST /<nym>/pos/invoice` with amount and optional memo
3. Navigate to `/#/pay/:id` with the invoice response
4. Show QR and status

## Payment screen (`/#/pay/:id`)

See `05-shared-components.md` — shared with donation mode.

On paid:
1. Navigate to `/#/receipt/:id`
2. Refresh server-authoritative invoice status/history
3. Play success sound + confetti + haptic

"New sale" button → back to `/#/`

## Receipt screen (`/#/receipt/:id`)

```
┌─────────────────────────────────┐
│         Seguras Butcher         │
│           Counter 1             │
│                                 │
│  ₡8,500                         │
│  0.00010800 BTC                 │
│  Rate: ₡78,703,124/BTC          │
│                                 │
│  Paid via: Lightning            │
│  2026-07-01  14:32              │
│                                 │
│  Note: Mesa 4                   │
│                                 │
│  [🖨 Print]  [↗ Share]          │
│                                 │
│  [New Sale]                     │
└─────────────────────────────────┘
```

Fields:
- Merchant header (from config)
- Fiat amount + currency
- Sats amount
- Exchange rate used (from invoice response)
- Rail (Lightning / Liquid / Bolt Card)
- Timestamp
- Note (if any)
- Print and Share CTAs
- New Sale

Receipt data comes from server invoice status/history. Terminal-created memos
are public receipt fields (`memo_public = true`) and are returned by the status
endpoint when present.

## Transaction history (`/#/history`)

Pull-up sheet accessible from the keypad screen. Also a full-page route for
navigation from the receipt screen.

```
Recent Transactions
───────────────────────────────────
14:32  ₡8,500   ✓ Paid     Lightning
14:18  ₡2,000   ✓ Paid     Liquid
13:55  ₡3,300   ✗ Expired  Lightning
13:44  ₡1,200   ✓ Paid     Bolt Card
```

- Source: `GET /<nym>/pos/invoices`, authenticated with the paired terminal token
- Stored fields come from the server response, including `id`, `fiat_amount_minor`,
  `fiat_currency`, `amount_sat`, `paid_via`, `status`, `paid_at_unix`, `memo`, and
  `rate_minor_per_btc`
- Tap row → receipt screen for that invoice (if paid) or status (if pending)
- Renders from cached state first, then refreshes from the server
- Server paging uses `page` and `pageSize`; the UI can keep only a small in-memory
  cache

## Settings screen (`/#/settings`) — PIN-gated

PIN gate: 4-digit PIN stored in `localStorage` as bcrypt hash (or scrypt — keep it
simple; this is a local-only check, not server auth). If no PIN set, settings are
unlocked with a single tap confirmation.

Settings contents:
- Display currency selector
- "About this terminal" (nym, server domain, terminal label/id)
- "Unpair terminal" (clears local token and returns to the pairing gate)
- Bolt Card toggle (show/hide NFC tab on payment screen)
- "Reset terminal" (clears local token/cache and returns to the pairing gate)

No relay config, no descriptor, no Nostr — none of that exists here.

## State management

Three Svelte stores plus terminal session state:

```ts
// stores/config.ts
// Read from injected JSON at boot, immutable
export const config = readable<BullnymConfig>(parseConfig())

// stores/invoice.ts
// Current in-flight invoice; cleared on paid/cancelled
export const currentInvoice = writable<Invoice | null>(null)

// stores/history.ts
// Server-backed; cache only the last loaded page in memory
export const history = writable<HistoryRecord[]>([])
```

The terminal token is client-generated, stored locally, and sent as a bearer
token after pairing. The server stores only the token hash. The full pairing,
history, memo, revocation, and 401 re-pair contract is in
`docs/features/pos-terminals.md`.
