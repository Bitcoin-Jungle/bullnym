// Typed client for the bullnym anonymous checkout endpoints, plus the POS
// terminal pairing/invoice endpoints (src/pos.rs). Shapes mirror the Rust
// handlers exactly — do not "improve" field names here.

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
    public code?: string,
  ) {
    super(message)
  }
  get isRateLimited(): boolean {
    return this.status === 429
  }
}

export interface CreateInvoiceRequest {
  amount_sat?: number
  fiat_amount_minor?: number
  fiat_currency?: string
}

/** Response of POST /<nym>/invoice (invoice.rs CreateInvoiceResponse). */
export interface CreateInvoiceResponse {
  invoice_id: string
  lightning_pr: string
  liquid_address: string
  bitcoin_chain_address: string | null
  bitcoin_chain_bip21: string | null
  expires_at_unix: number
}

/** Response of GET /api/v1/invoices/:id/status (InvoiceStatusResponse). */
export interface InvoiceStatus {
  status:
    | 'pending'
    | 'partially_paid'
    | 'paid'
    | 'overpaid'
    | 'underpaid'
    | 'expired'
    | 'cancelled'
    | string
  /** Populated only for POS-created invoices (memo_public), in every lifecycle state. */
  memo: string | null
  pricing_mode: string
  settlement_status: string
  amount_sat: number
  fiat_amount_minor: number | null
  fiat_currency: string | null
  remaining_amount_sat: number
  payment_tolerance_sat: number
  rate_minor_per_btc: number | null
  rate_locks_until_unix: number
  expires_at_unix: number
  paid_via: string | null
  paid_at_unix: number | null
  paid_amount_sat: number | null
  lightning_pr: string | null
  liquid_address: string | null
  bitcoin_address: string | null
  bitcoin_chain_address: string | null
  bitcoin_chain_bip21: string | null
  accept_btc: boolean
  accept_ln: boolean
  accept_liquid: boolean
}

export interface CurrencyView {
  code: string
  precision: number
}

export interface SupportedCurrenciesResponse {
  currencies: CurrencyView[]
}

// Per src/error.rs: the server deliberately returns HTTP 200 with an
// LNURL-style (LUD-06) error envelope — {"status":"ERROR","code":"...",
// "reason":"..."} — for nearly all error conditions, across nearly every
// endpoint (POST /:nym/invoice and the status endpoint included). Only
// AuthError (401), the two address-already-used variants (409), and
// ServiceUnavailable (503) get a real non-2xx status; everything else is a
// 200 whose body needs to be inspected to detect failure. Before this fix,
// request() only checked res.ok, so every such envelope parsed as success
// — this was the true origin of the "createInvoice succeeds with
// invoice_id undefined, app navigates to /#/pay/undefined and polls
// forever" bug: the envelope has no invoice_id, so CreateInvoiceResponse
// came back with invoice_id === undefined.
const NOT_FOUND_CODES = new Set(['InvoiceNotFound', 'DonationPageNotFound', 'NymNotFound'])
const RATE_LIMITED_CODES = new Set(['RateLimitedSender', 'RateLimitedRecipient', 'RateLimitedNetwork'])

interface ErrorEnvelope {
  status: 'ERROR'
  code?: string
  reason?: string
}

function isErrorEnvelope(body: unknown): body is ErrorEnvelope {
  return typeof body === 'object' && body !== null && (body as { status?: unknown }).status === 'ERROR'
}

function envelopeHttpStatus(code: string | undefined): number {
  if (code && NOT_FOUND_CODES.has(code)) return 404
  if (code && RATE_LIMITED_CODES.has(code)) return 429
  return 400
}

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  let res: Response
  try {
    res = await fetch(url, init)
  } catch {
    throw new ApiError(0, 'Server unreachable')
  }
  if (!res.ok) {
    let msg = res.statusText
    try {
      msg = await res.text()
    } catch {
      /* keep statusText */
    }
    throw new ApiError(res.status, msg)
  }
  const body = (await res.json()) as unknown
  if (isErrorEnvelope(body)) {
    throw new ApiError(envelopeHttpStatus(body.code), body.reason ?? body.code ?? 'Request failed', body.code)
  }
  return body as T
}

export function createInvoice(
  nym: string,
  req: CreateInvoiceRequest,
): Promise<CreateInvoiceResponse> {
  return request(`/${nym}/invoice`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(req),
  })
}

export function getInvoiceStatus(id: string): Promise<InvoiceStatus> {
  return request(`/api/v1/invoices/${id}/status`)
}

export function getSupportedCurrencies(): Promise<SupportedCurrenciesResponse> {
  return request('/api/v1/supported-currencies')
}

/**
 * Requests (or re-requests) a fresh Lightning offer for an invoice. Used
 * both for the initial offer (when the create response seeded lightning_pr
 * as '', e.g. on deep-link reconstruction) and to replace an offer that
 * expired mid-payment. On a non-payable/error invoice the server returns
 * the LNURL error envelope, which request() already converts into a thrown
 * ApiError — callers catch it (see PaymentScreen.svelte's throttled
 * maybeRefreshLightning()).
 */
export function fetchLightningOffer(id: string): Promise<{ pr: string }> {
  return request(`/api/v1/invoices/${id}/lightning`, { method: 'POST' })
}

// ---------------------------------------------------------------------------
// POS terminal endpoints (src/pos.rs). Pairing is anonymous (proven by
// token_hash, not a bearer token); create/list/cancel require the bearer
// terminal token.
// ---------------------------------------------------------------------------

/** Response of POST /:nym/pos/pairings (pos.rs CreatePairingResponse). */
export interface CreatePairingResponse {
  pairing_id: string
  code: string
  expires_at_unix: number
}

export function createPairing(nym: string, tokenHash: string): Promise<CreatePairingResponse> {
  return request(`/${nym}/pos/pairings`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ token_hash: tokenHash }),
  })
}

/** Response of GET /:nym/pos/pairings/:id (pos.rs PollPairingResponse). */
export interface PollPairingResponse {
  status: 'pending' | 'approved' | 'expired' | string
  terminal_id?: string
}

export function pollPairing(nym: string, pairingId: string, tokenHash: string): Promise<PollPairingResponse> {
  return request(`/${nym}/pos/pairings/${pairingId}?token_hash=${encodeURIComponent(tokenHash)}`)
}

/**
 * Called whenever a terminal-authed request (bearer token) comes back 401 —
 * the token was never valid, expired, or the wallet revoked it server-side.
 * client.ts deliberately does NOT import the terminal store or router here
 * (that would be a circular import: the store imports this module for
 * createPairing/pollPairing) — the POS app registers a handler at boot
 * instead (apps/pos/App.svelte), which clears pairing state and lets the
 * pairing gate take over.
 */
type UnauthorizedHandler = () => void
let onUnauthorized: UnauthorizedHandler | null = null

export function registerUnauthorizedHandler(handler: UnauthorizedHandler): void {
  onUnauthorized = handler
}

async function terminalRequest<T>(token: string, url: string, init?: RequestInit): Promise<T> {
  try {
    return await request<T>(url, {
      ...init,
      headers: { ...(init?.headers ?? {}), Authorization: `Bearer ${token}` },
    })
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) onUnauthorized?.()
    throw err
  }
}

export interface CreateTerminalInvoiceRequest {
  amount_sat?: number
  fiat_amount_minor?: number
  fiat_currency?: string
  /** <=280 chars, no control characters (validated client-side; server re-validates). */
  memo?: string
}

/** POST /:nym/pos/invoice — same response shape as the anonymous create. */
export function createTerminalInvoice(
  nym: string,
  token: string,
  req: CreateTerminalInvoiceRequest,
): Promise<CreateInvoiceResponse> {
  return terminalRequest(token, `/${nym}/pos/invoice`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(req),
  })
}

/** Item shape of GET /:nym/pos/invoices (pos.rs PosInvoiceListItem). */
export interface PosInvoiceListItem {
  id: string
  status: string
  pricing_mode: string
  settlement_status: string
  amount_sat: number
  remaining_amount_sat: number
  fiat_amount_minor: number | null
  fiat_currency: string | null
  memo: string | null
  terminal_id: string | null
  accept_btc: boolean
  accept_ln: boolean
  accept_liquid: boolean
  bitcoin_address: string | null
  liquid_address: string | null
  created_at_unix: number
  expires_at_unix: number
  paid_via: string | null
  paid_at_unix: number | null
  paid_amount_sat: number | null
}

/** Response of GET /:nym/pos/invoices (pos.rs ListInvoicesResponse). */
export interface ListInvoicesResponse {
  invoices: PosInvoiceListItem[]
  page: number
  pageSize: number
  has_more: boolean
}

export interface ListTerminalInvoicesParams {
  page: number
  pageSize: number
  status?: string
}

export function listTerminalInvoices(
  nym: string,
  token: string,
  params: ListTerminalInvoicesParams,
): Promise<ListInvoicesResponse> {
  const qs = new URLSearchParams({ page: String(params.page), pageSize: String(params.pageSize) })
  if (params.status) qs.set('status', params.status)
  return terminalRequest(token, `/${nym}/pos/invoices?${qs.toString()}`)
}

/** Response of POST /:nym/pos/invoices/:id/cancel (pos.rs CancelInvoiceResponse). */
export interface CancelTerminalInvoiceResponse {
  invoice_id: string
  status: string
}

/**
 * Cancels an unpaid terminal-attributed invoice. Rejects with
 * ApiError.code === 'InvoicePaymentAlreadyDetected' when payment was already
 * detected server-side (in_progress/partially_paid/paid/underpaid/overpaid) —
 * callers must surface that case distinctly rather than as a generic error.
 */
export function cancelTerminalInvoice(nym: string, token: string, id: string): Promise<CancelTerminalInvoiceResponse> {
  return terminalRequest(token, `/${nym}/pos/invoices/${id}/cancel`, { method: 'POST' })
}
