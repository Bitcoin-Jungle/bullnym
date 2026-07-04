<script lang="ts">
  // Reskinned to nostr-pos's Transactions.svelte
  // (~/apps/nostr-pos/apps/pos-pwa/src/routes/Transactions.svelte), rendered
  // with the ported TransactionSheet component. Server-backed now (POS
  // history is no longer a local-only store — see
  // .context/pos-terminal-plan.md's "History goes server-backed"): fetches
  // GET /:nym/pos/invoices with the paired terminal's bearer token, paging
  // via has_more.
  import { Settings } from 'lucide-svelte'
  import { config } from '$lib/config'
  import { terminal } from '$lib/stores/terminal.svelte'
  import {
    listTerminalInvoices,
    getSupportedCurrencies,
    ApiError,
    type PosInvoiceListItem,
    type CurrencyView,
  } from '$lib/api/client'
  import TransactionSheet from '$lib/components/TransactionSheet.svelte'
  import BullFooter from '$lib/components/BullFooter.svelte'
  import BullSpinner from '$lib/components/BullSpinner.svelte'
  import Button from '$lib/components/Button.svelte'

  const PAGE_SIZE = 50

  type LoadState = 'loading' | 'ready' | 'error'

  let loadState = $state<LoadState>('loading')
  let errorMsg = $state('')
  let items = $state<PosInvoiceListItem[]>([])
  let page = $state(1)
  let hasMore = $state(false)
  let loadingMore = $state(false)
  let currencies = $state<CurrencyView[]>([])

  getSupportedCurrencies()
    .then((res) => {
      currencies = res.currencies
    })
    .catch(() => {
      /* keep empty; invoiceAmountLabel falls back to precision 2 */
    })

  async function load(targetPage: number, append: boolean): Promise<void> {
    if (!terminal.token) {
      errorMsg = 'Not paired'
      loadState = 'error'
      return
    }
    try {
      const res = await listTerminalInvoices(config.nym, terminal.token, { page: targetPage, pageSize: PAGE_SIZE })
      items = append ? [...items, ...res.invoices] : res.invoices
      hasMore = res.has_more
      page = targetPage
      loadState = 'ready'
    } catch (err) {
      errorMsg = err instanceof ApiError ? err.message || 'Could not load transactions' : 'Could not load transactions'
      loadState = 'error'
    } finally {
      loadingMore = false
    }
  }

  void load(1, false)

  function retry(): void {
    loadState = 'loading'
    void load(1, false)
  }

  async function loadMore(): Promise<void> {
    if (loadingMore || !hasMore) return
    loadingMore = true
    await load(page + 1, true)
  }
</script>

<main class="min-h-screen bg-[#f5f0e8] px-5 py-5 text-[#211f1a] dark:bg-[#161512] dark:text-[#fff6e8]">
  <div class="mx-auto max-w-3xl">
    <header class="mb-8 flex items-center justify-between gap-4">
      <a class="inline-flex min-h-12 items-center gap-2 rounded-md px-2 text-sm font-semibold" href="#/">← Back</a>
      <a
        class="grid min-h-12 min-w-12 place-items-center rounded-md bg-[#eadfce] text-[#211f1a] dark:bg-[#2c2922] dark:text-[#fff6e8]"
        href="#/settings"
        aria-label="Settings"
      >
        <Settings size={22} />
      </a>
    </header>

    <div class="mb-5">
      <h1 class="font-display text-4xl uppercase tracking-display leading-none">Recent transactions</h1>
      <p class="mt-1 text-xs font-medium uppercase tracking-[0.12em] text-[#776b5a] dark:text-[#b9aa91]">
        {config.header || config.nym}
      </p>
    </div>

    {#if loadState === 'loading'}
      <div class="grid min-h-[40vh] place-items-center">
        <BullSpinner size={64} label="Loading transactions" />
      </div>
    {:else if loadState === 'error'}
      <div class="rounded-lg bg-[#ffe0d9] p-5 text-[#8c2d28]">
        <p class="font-semibold">Could not load transactions.</p>
        <p class="mt-1 text-sm">{errorMsg}</p>
        <div class="mt-4"><Button onclick={retry}>Try again</Button></div>
      </div>
    {:else}
      <TransactionSheet rows={items} {currencies} />
      {#if hasMore}
        <div class="mt-4 flex justify-center">
          <Button variant="secondary" disabled={loadingMore} onclick={loadMore}>
            {loadingMore ? 'Loading…' : 'Load more'}
          </Button>
        </div>
      {/if}
    {/if}

    <BullFooter />
  </div>
</main>
