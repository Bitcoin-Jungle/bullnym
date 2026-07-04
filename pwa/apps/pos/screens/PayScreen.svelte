<script lang="ts">
  // Thin wrapper over PayFlow.svelte (review item 3): supplies POS's header
  // row (Cancel sale + History/Settings icons), success actions (New Sale +
  // Print Receipt), and exit navigation (back to the keypad). All
  // invoice-loading, polling, and terminal-panel rendering lives in
  // PayFlow.svelte + PaymentScreen.svelte.
  //
  // History is server-backed now (see .context/pos-terminal-plan.md) — the
  // old onPaid -> history.add(...) side effect is gone; the server is the
  // only record. "Cancel sale" now actually cancels the terminal invoice
  // server-side (POST /:nym/pos/invoices/:id/cancel) instead of just
  // navigating away, so an abandoned unpaid sale doesn't linger as
  // "unpaid" in history forever. If payment was already detected
  // server-side, cancellation is refused (InvoicePaymentAlreadyDetected) —
  // surfaced distinctly so the merchant isn't told a sale was cancelled
  // when money actually moved. Tapping the button again after that error
  // just exits (matches the two-tap confirm pattern used elsewhere in the
  // app, e.g. SettingsScreen's reset/unpair).
  import { History, Settings } from 'lucide-svelte'
  import { config } from '$lib/config'
  import { cancelTerminalInvoice, ApiError } from '$lib/api/client'
  import { terminal } from '$lib/stores/terminal.svelte'
  import { router } from '$lib/router.svelte'
  import PayFlow from '$lib/components/PayFlow.svelte'

  let { id }: { id: string } = $props()

  let cancelling = $state(false)
  let cancelError = $state<string | null>(null)

  function newSale() {
    router.go('/')
  }

  async function cancelSale() {
    if (cancelling) return
    if (cancelError) {
      // Second tap after an error just exits — the cancel attempt already
      // told the merchant what happened.
      newSale()
      return
    }
    if (!terminal.token) {
      newSale()
      return
    }
    cancelling = true
    try {
      await cancelTerminalInvoice(config.nym, terminal.token, id)
      newSale()
    } catch (err) {
      if (err instanceof ApiError && err.code === 'InvoicePaymentAlreadyDetected') {
        cancelError = 'Payment already detected — cannot cancel'
      } else if (err instanceof ApiError && err.code === 'InvoiceNotFound') {
        // Already cancelled/expired/foreign — nothing left to do here.
        newSale()
        return
      } else {
        cancelError = err instanceof ApiError ? err.message || 'Could not cancel sale' : 'Could not cancel sale'
      }
    } finally {
      cancelling = false
    }
  }

  function goToReceipt() {
    router.go(`/receipt/${id}`)
  }
</script>

{#snippet header()}
  <header class="mb-6 flex flex-col gap-3">
    <div class="flex items-center justify-between">
      <button
        type="button"
        class="inline-flex min-h-12 items-center gap-2 rounded-md px-2 text-sm font-semibold disabled:opacity-50"
        disabled={cancelling}
        onclick={cancelSale}
      >
        ← {cancelling ? 'Cancelling…' : 'Cancel sale'}
      </button>
      <div class="flex items-center gap-2">
        <a
          class="grid min-h-12 min-w-12 place-items-center rounded-md bg-[#eadfce] text-[#211f1a] dark:bg-[#2c2922] dark:text-[#fff6e8]"
          href="#/history"
          aria-label="Recent transactions"
        >
          <History size={22} />
        </a>
        <a
          class="grid min-h-12 min-w-12 place-items-center rounded-md bg-[#eadfce] text-[#211f1a] dark:bg-[#2c2922] dark:text-[#fff6e8]"
          href="#/settings"
          aria-label="Settings"
        >
          <Settings size={22} />
        </a>
      </div>
    </div>
    {#if cancelError}
      <p class="rounded-md bg-[#ffe0d9] px-4 py-3 text-sm font-semibold text-[#8c2d28]">{cancelError}</p>
    {/if}
  </header>
{/snippet}

<PayFlow
  {id}
  {header}
  successActionLabel="New Sale"
  onSuccessAction={newSale}
  successSecondaryLabel="Print Receipt"
  onSuccessSecondary={goToReceipt}
  onExit={newSale}
/>
