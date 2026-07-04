// Pure server-item -> display mapping shared by TransactionSheet.svelte
// (list rows) and ReceiptScreen.svelte (single-invoice fallback): both need
// to resolve a currency code to its real display precision (CRC is
// precision 0 and is a primary market — defaulting to 2 would silently
// misprint amounts by 100x) and format the fiat amount consistently.
// Extracted as plain functions (not $derived inline in the components) so
// the mapping is unit-testable without a component-testing harness, which
// this project deliberately doesn't have (see status.ts/rails.ts for the
// same pattern).
import type { CurrencyView } from '$lib/api/client'
import { formatFiat } from '$lib/money'

export function precisionFor(currency: string | null | undefined, currencies: CurrencyView[]): number {
  if (!currency) return 2
  return currencies.find((c) => c.code === currency)?.precision ?? 2
}

export function invoiceAmountLabel(
  item: { fiat_amount_minor: number | null; fiat_currency: string | null },
  currencies: CurrencyView[],
): string {
  if (!item.fiat_currency) return '—'
  return formatFiat(item.fiat_amount_minor ?? 0, item.fiat_currency, precisionFor(item.fiat_currency, currencies))
}
