// Terminal store: token generation/hashing, localStorage persistence, and
// pairing-flow state transitions. The store is a module-scope singleton
// (same pattern as settings.svelte.ts/rate.svelte.ts) that reads its
// initial value from localStorage at import time, so every test re-imports
// it fresh (vi.resetModules()) after stubbing a clean in-memory
// localStorage — matching config.test.ts's vi.stubGlobal approach, since
// Node's default vitest environment has no real localStorage.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

function fakeLocalStorage(): Storage {
  const data = new Map<string, string>()
  return {
    getItem: (key: string) => (data.has(key) ? data.get(key)! : null),
    setItem: (key: string, value: string) => {
      data.set(key, value)
    },
    removeItem: (key: string) => {
      data.delete(key)
    },
    clear: () => data.clear(),
    key: (index: number) => Array.from(data.keys())[index] ?? null,
    get length() {
      return data.size
    },
  } as Storage
}

function mockFetchOnce(body: unknown, status = 200): void {
  vi.stubGlobal(
    'fetch',
    vi.fn().mockResolvedValue({
      ok: status >= 200 && status < 300,
      status,
      statusText: 'irrelevant',
      json: () => Promise.resolve(body),
      text: () => Promise.resolve(JSON.stringify(body)),
    }),
  )
}

beforeEach(() => {
  vi.stubGlobal('localStorage', fakeLocalStorage())
  vi.resetModules()
})

afterEach(() => {
  vi.unstubAllGlobals()
  vi.useRealTimers()
})

describe('terminal store', () => {
  it('starts unpaired with no token/terminalId/label', async () => {
    const { terminal } = await import('./terminal.svelte')
    expect(terminal.paired).toBe(false)
    expect(terminal.token).toBeNull()
    expect(terminal.terminalId).toBeNull()
    expect(terminal.label).toBeNull()
    expect(terminal.pairing).toEqual({ kind: 'idle' })
  })

  it('startPairing generates a 64-char lowercase-hex token hash and sends it, never the raw token', async () => {
    mockFetchOnce({ pairing_id: 'p1', code: 'ABCD1234', expires_at_unix: 1_700_000_300 })
    const { terminal } = await import('./terminal.svelte')

    await terminal.startPairing()

    expect(fetch).toHaveBeenCalledTimes(1)
    const [url, init] = vi.mocked(fetch).mock.calls[0]!
    expect(url).toMatch(/\/pos\/pairings$/)
    const body = JSON.parse((init as RequestInit).body as string) as { token_hash: string }
    expect(body.token_hash).toMatch(/^[0-9a-f]{64}$/)

    expect(terminal.pairing).toMatchObject({ kind: 'pairing', pairingId: 'p1', code: 'ABCD1234', expiresAtUnix: 1_700_000_300 })
  })

  it('poll transitions to approved: persists token+terminalId and returns to idle pairing state', async () => {
    vi.useFakeTimers()
    mockFetchOnce({ pairing_id: 'p1', code: 'ABCD1234', expires_at_unix: 1_700_000_300 })
    const { terminal } = await import('./terminal.svelte')

    await terminal.startPairing()
    expect(terminal.paired).toBe(false)

    mockFetchOnce({ status: 'approved', terminal_id: 't-123' })
    await vi.advanceTimersByTimeAsync(2000)

    expect(terminal.paired).toBe(true)
    expect(terminal.terminalId).toBe('t-123')
    expect(terminal.token).toMatch(/^[0-9a-f]{64}$/)
    expect(terminal.pairing).toEqual({ kind: 'idle' })
  })

  it('poll transitions to expired when the server reports the code expired', async () => {
    vi.useFakeTimers()
    mockFetchOnce({ pairing_id: 'p1', code: 'ABCD1234', expires_at_unix: 1_700_000_300 })
    const { terminal } = await import('./terminal.svelte')

    await terminal.startPairing()

    mockFetchOnce({ status: 'expired' })
    await vi.advanceTimersByTimeAsync(2000)

    expect(terminal.pairing).toEqual({ kind: 'expired' })
    expect(terminal.paired).toBe(false)
  })

  it('a createPairing failure surfaces as a pairing error state', async () => {
    mockFetchOnce({ status: 'ERROR', code: 'RateLimitedSender', reason: 'rate limited (sender)' })
    const { terminal } = await import('./terminal.svelte')

    await terminal.startPairing()

    expect(terminal.pairing.kind).toBe('error')
  })

  it('persists across a fresh import of the module (localStorage round-trip)', async () => {
    vi.useFakeTimers()
    mockFetchOnce({ pairing_id: 'p1', code: 'ABCD1234', expires_at_unix: 1_700_000_300 })
    const first = await import('./terminal.svelte')
    await first.terminal.startPairing()
    mockFetchOnce({ status: 'approved', terminal_id: 't-abc' })
    await vi.advanceTimersByTimeAsync(2000)
    expect(first.terminal.paired).toBe(true)

    vi.resetModules()
    const second = await import('./terminal.svelte')
    expect(second.terminal.paired).toBe(true)
    expect(second.terminal.terminalId).toBe('t-abc')
  })

  it('unpair clears the terminal record but leaves other bullnym: keys untouched', async () => {
    vi.useFakeTimers()
    localStorage.setItem('bullnym:settings::currency', JSON.stringify('CRC'))
    mockFetchOnce({ pairing_id: 'p1', code: 'ABCD1234', expires_at_unix: 1_700_000_300 })
    const { terminal } = await import('./terminal.svelte')
    await terminal.startPairing()
    mockFetchOnce({ status: 'approved', terminal_id: 't-1' })
    await vi.advanceTimersByTimeAsync(2000)
    expect(terminal.paired).toBe(true)

    terminal.unpair()

    expect(terminal.paired).toBe(false)
    expect(terminal.token).toBeNull()
    expect(terminal.terminalId).toBeNull()
    expect(localStorage.getItem('bullnym:settings::currency')).toBe(JSON.stringify('CRC'))
  })
})
