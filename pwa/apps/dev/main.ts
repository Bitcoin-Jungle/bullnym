// Dev-only harness for the P1 fix (chain-swap Bitcoin rail must not be
// gated on accept_btc). Stubs the backend so we can drive PaymentScreen
// through the exact regression scenario in a real browser without the Rust
// server (which mints live Boltz swaps on invoice creation).
//
// Run:  npm run dev  →  open /pwa-assets/apps/dev/index.html
import { mount } from 'svelte'
import '$lib/app.css'
import PaymentScreen from '$lib/components/PaymentScreen.svelte'
import type { CreateInvoiceResponse, InvoiceStatus } from '$lib/api/client'

const INVOICE_ID = 'dev-chain-swap-001'
const AMOUNT_SAT = 15_384
const CHAIN_ADDR = 'bc1qchainswaplockupaddressexampledev0000000'
const CHAIN_BIP21 = `bitcoin:${CHAIN_ADDR}?amount=0.00015384&label=devtest`
const LN_PR = 'lnbc153840n1devplaceholderpaymentrequestxxxxxxxxxxxxxxxxxxxxxxxx'
const LQ_ADDR = 'lq1qwliquidaddressexampledev00000000000000000000000000'

// The regression status: direct BTC disabled, but a chain-swap offer is
// present because the invoice has a Liquid address. Before the fix, the
// Bitcoin tab vanished on the first poll (accept_btc=false); after the fix
// it stays, because the chain-swap address is payable regardless.
const STATUS: InvoiceStatus = {
  status: 'pending',
  memo: null,
  pricing_mode: 'fiat',
  settlement_status: 'unpaid',
  amount_sat: AMOUNT_SAT,
  fiat_amount_minor: 100,
  fiat_currency: 'USD',
  remaining_amount_sat: AMOUNT_SAT,
  payment_tolerance_sat: 0,
  rate_minor_per_btc: 6_500_000,
  rate_locks_until_unix: Math.floor(Date.now() / 1000) + 600,
  expires_at_unix: Math.floor(Date.now() / 1000) + 600,
  paid_via: null,
  paid_at_unix: null,
  paid_amount_sat: null,
  lightning_pr: LN_PR,
  liquid_address: LQ_ADDR,
  bitcoin_address: null,
  bitcoin_chain_address: CHAIN_ADDR,
  bitcoin_chain_bip21: CHAIN_BIP21,
  accept_btc: false,
  accept_ln: true,
  accept_liquid: true,
}

const realFetch = window.fetch.bind(window)
window.fetch = (input: RequestInfo | URL, init?: RequestInit) => {
  const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
  if (url.includes(`/api/v1/invoices/${INVOICE_ID}/status`)) {
    return Promise.resolve(new Response(JSON.stringify(STATUS), { status: 200, headers: { 'content-type': 'application/json' } }))
  }
  if (url.includes(`/api/v1/invoices/${INVOICE_ID}/lightning`)) {
    return Promise.resolve(new Response(JSON.stringify({ pr: LN_PR }), { status: 200, headers: { 'content-type': 'application/json' } }))
  }
  // Everything else (liquid ws upgrade, etc.) falls through to the real
  // fetch so failures surface honestly rather than being masked.
  return realFetch(input as RequestInfo, init)
}

const invoice: CreateInvoiceResponse = {
  invoice_id: INVOICE_ID,
  lightning_pr: LN_PR,
  liquid_address: LQ_ADDR,
  bitcoin_chain_address: CHAIN_ADDR,
  bitcoin_chain_bip21: CHAIN_BIP21,
  expires_at_unix: STATUS.expires_at_unix,
}

const target = document.getElementById('app')
if (!target) throw new Error('missing #app root')

mount(PaymentScreen, {
  target,
  props: {
    invoice,
    nym: 'devtest',
    amountLabel: '$1.00',
    onTerminal: (t) => console.log('[dev-harness] onTerminal', t),
  },
})

console.log('[dev-harness] mounted. Expect a Bitcoin tab to remain after the first poll (accept_btc=false + chain swap).')
