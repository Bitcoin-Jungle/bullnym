// POS terminal identity + pairing lifecycle. Server-authoritative: the PWA
// holds no keys, only a bearer token endorsed by the merchant's wallet (see
// .context/pos-terminal-plan.md's "Token model"/"Pairing flow" sections).
//
// Token generation mirrors lib/pin.ts's WebCrypto SHA-256 pattern: 32 random
// bytes -> lowercase hex is the token itself (never sent to the server —
// only its hash is), hashed again with SHA-256 for the wire value.
//
// Persistence follows local.svelte.ts's per-nym localStorage convention
// (like settings.svelte.ts / lib/pin.ts): a single JSON blob under
// `bullnym:terminal:<nym>` so Settings' "reset terminal" (which wipes every
// `bullnym:`-prefixed key) unpairs for free, and "unpair terminal" only has
// to touch this one key.

import { localStore } from '$lib/stores/local.svelte'
import { config } from '$lib/config'
import { ApiError, createPairing, pollPairing } from '$lib/api/client'

const POLL_MS = 2000

export interface TerminalRecord {
  token: string | null
  terminalId: string | null
  label: string | null
}

function terminalKey(nym: string): string {
  return `bullnym:terminal:${nym}`
}

const record = localStore<TerminalRecord>(terminalKey(config.nym), {
  token: null,
  terminalId: null,
  label: null,
})

export type PairingState =
  | { kind: 'idle' }
  | { kind: 'starting' }
  | { kind: 'pairing'; pairingId: string; code: string; expiresAtUnix: number }
  | { kind: 'expired' }
  | { kind: 'error'; message: string }

let pairingState = $state<PairingState>({ kind: 'idle' })
let pollTimer: ReturnType<typeof setInterval> | undefined
/** The raw token for the in-flight pairing attempt — only persisted once the server confirms approval. */
let pendingToken: string | null = null

function randomHex(byteLength: number): string {
  const bytes = new Uint8Array(byteLength)
  crypto.getRandomValues(bytes)
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('')
}

async function sha256Hex(input: string): Promise<string> {
  const data = new TextEncoder().encode(input)
  const digest = await crypto.subtle.digest('SHA-256', data)
  return Array.from(new Uint8Array(digest))
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('')
}

function stopPolling(): void {
  if (pollTimer) clearInterval(pollTimer)
  pollTimer = undefined
}

async function pollOnce(pairingId: string, tokenHash: string): Promise<void> {
  try {
    const res = await pollPairing(config.nym, pairingId, tokenHash)
    if (res.status === 'approved' && res.terminal_id) {
      stopPolling()
      record.value = { token: pendingToken, terminalId: res.terminal_id, label: null }
      pendingToken = null
      pairingState = { kind: 'idle' }
      return
    }
    if (res.status === 'expired') {
      stopPolling()
      pairingState = { kind: 'expired' }
    }
    // status === 'pending': keep polling, current pairingState already shows it.
  } catch {
    // Transient network hiccup — keep polling rather than surfacing an
    // error for every dropped poll; the pairing code's own expiry is the
    // backstop if the server is genuinely unreachable.
  }
}

export const terminal = {
  get paired(): boolean {
    return record.value.terminalId !== null
  },
  get token(): string | null {
    return record.value.token
  },
  get terminalId(): string | null {
    return record.value.terminalId
  },
  get label(): string | null {
    return record.value.label
  },
  get pairing(): PairingState {
    return pairingState
  },

  /** Requests a fresh pairing code and starts polling for approval. Safe to call again (e.g. "get new code") — replaces any in-flight attempt. */
  async startPairing(): Promise<void> {
    stopPolling()
    pairingState = { kind: 'starting' }
    const token = randomHex(32)
    const tokenHash = await sha256Hex(token)
    try {
      const res = await createPairing(config.nym, tokenHash)
      pendingToken = token
      pairingState = { kind: 'pairing', pairingId: res.pairing_id, code: res.code, expiresAtUnix: res.expires_at_unix }
      pollTimer = setInterval(() => void pollOnce(res.pairing_id, tokenHash), POLL_MS)
    } catch (err) {
      pairingState = { kind: 'error', message: err instanceof ApiError ? err.message : 'Could not start pairing' }
    }
  },

  /** Clears only the terminal identity — PIN/settings/other bullnym: keys survive. Called on manual unpair and on a 401 from a terminal-authed call (see client.ts's onUnauthorized). */
  unpair(): void {
    stopPolling()
    pendingToken = null
    pairingState = { kind: 'idle' }
    record.value = { token: null, terminalId: null, label: null }
  },
}
