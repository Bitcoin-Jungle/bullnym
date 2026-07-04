import { describe, expect, it } from 'vitest'
import { precisionFor, invoiceAmountLabel } from './pos-history'
import { formatFiat } from './money'
import type { CurrencyView } from './api/client'

const CURRENCIES: CurrencyView[] = [
  { code: 'USD', precision: 2 },
  { code: 'CRC', precision: 0 },
]

describe('precisionFor', () => {
  it('resolves a known currency to its real precision', () => {
    expect(precisionFor('CRC', CURRENCIES)).toBe(0)
    expect(precisionFor('USD', CURRENCIES)).toBe(2)
  })

  it('defaults to 2 for an unknown or missing currency', () => {
    expect(precisionFor('EUR', CURRENCIES)).toBe(2)
    expect(precisionFor(null, CURRENCIES)).toBe(2)
    expect(precisionFor(undefined, CURRENCIES)).toBe(2)
  })
})

describe('invoiceAmountLabel', () => {
  it('formats using the real currency precision (not a hardcoded 2)', () => {
    expect(invoiceAmountLabel({ fiat_amount_minor: 850_000, fiat_currency: 'CRC' }, CURRENCIES)).toBe(
      formatFiat(850_000, 'CRC', 0),
    )
  })

  it('treats a null fiat_amount_minor as 0', () => {
    expect(invoiceAmountLabel({ fiat_amount_minor: null, fiat_currency: 'USD' }, CURRENCIES)).toBe(
      formatFiat(0, 'USD', 2),
    )
  })

  it('renders an em dash for a currency-less (sat/BTC) item', () => {
    expect(invoiceAmountLabel({ fiat_amount_minor: null, fiat_currency: null }, CURRENCIES)).toBe('—')
  })
})
