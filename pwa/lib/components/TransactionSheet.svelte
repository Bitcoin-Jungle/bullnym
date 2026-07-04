<script lang="ts">
  // Ported from nostr-pos
  // (~/apps/nostr-pos/apps/pos-pwa/src/lib/ui/TransactionSheet.svelte).
  // Same markup/classes; `rows` is now the server's PosInvoiceListItem[]
  // (GET /:nym/pos/invoices, lib/api/client.ts) instead of the old
  // localStorage-only HistoryRecord[] — history is server-backed now (see
  // apps/pos/screens/HistoryScreen.svelte), so this renders across devices
  // and survives a local reset. Method label comes from our lib/rails.ts
  // instead of upstream's methodLabel(PaymentMethod); amount formatting
  // resolves the real currency precision via lib/pos-history.ts.
  import { ChevronRight } from 'lucide-svelte'
  import type { PosInvoiceListItem, CurrencyView } from '$lib/api/client'
  import { invoiceAmountLabel } from '$lib/pos-history'
  import { railLabel } from '$lib/rails'
  import { isTerminalPaid, statusBorderTone, statusLabel } from '$lib/status'

  let { rows, currencies = [] }: { rows: PosInvoiceListItem[]; currencies?: CurrencyView[] } = $props()
</script>

<section>
  {#if rows.length === 0}
    <p
      class="rounded-lg border border-[#d7c8b4] bg-[#fffaf0] py-12 text-center text-sm text-[#776b5a] dark:border-[#3a342a] dark:bg-[#211f1a] dark:text-[#b9aa91]"
    >
      Completed sales will appear here.
    </p>
  {:else}
    <div class="space-y-1.5">
      {#each rows as row (row.id)}
        <a
          class={`grid grid-cols-[1fr_auto] items-center gap-3 rounded-lg border-l-4 bg-[#fbf4e8] px-4 py-3 transition hover:bg-[#f3e7d6] dark:bg-[#26231d] ${statusBorderTone(row.status)}`}
          href={`#/receipt/${row.id}`}
        >
          <div class="min-w-0">
            <div class="flex items-baseline gap-2">
              <span class="font-black tabular-nums">
                {invoiceAmountLabel(row, currencies)}
              </span>
              {#if !isTerminalPaid(row.status)}
                <span class="text-xs font-semibold text-[#776b5a] dark:text-[#b9aa91]">{statusLabel(row.status)}</span>
              {/if}
            </div>
            <div class="mt-0.5 text-xs text-[#776b5a] dark:text-[#b9aa91]">
              {row.paid_at_unix
                ? new Date(row.paid_at_unix * 1000).toLocaleString([], {
                    month: 'short',
                    day: 'numeric',
                    hour: '2-digit',
                    minute: '2-digit',
                  })
                : '—'}
              &middot; {railLabel(row.paid_via)}
            </div>
          </div>
          <ChevronRight size={18} class="text-[#b1a287] dark:text-[#6d634f]" />
        </a>
      {/each}
    </div>
  {/if}
</section>
