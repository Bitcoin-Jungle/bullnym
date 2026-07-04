<script lang="ts">
  // POS mode shell — hash-routed over lib/router.svelte.ts. Deferred:
  // PWA manifest/service worker.
  //
  // Gated on terminal pairing (see lib/stores/terminal.svelte.ts): an
  // unpaired terminal only ever renders PairingScreen, regardless of the
  // current hash route — there's nothing useful behind the gate without a
  // bearer token. registerUnauthorizedHandler is wired here (component init
  // runs once at app boot) rather than in client.ts itself, which must not
  // import the terminal store (that store already imports client.ts for
  // createPairing/pollPairing — importing back would be circular).
  import { router } from '$lib/router.svelte'
  import { terminal } from '$lib/stores/terminal.svelte'
  import { registerUnauthorizedHandler } from '$lib/api/client'
  import PairingScreen from './screens/PairingScreen.svelte'
  import KeypadScreen from './screens/KeypadScreen.svelte'
  import PayScreen from './screens/PayScreen.svelte'
  import ReceiptScreen from './screens/ReceiptScreen.svelte'
  import HistoryScreen from './screens/HistoryScreen.svelte'
  import SettingsScreen from './screens/SettingsScreen.svelte'

  registerUnauthorizedHandler(() => terminal.unpair())

  const payId = $derived(router.match('/pay/:id')?.id)
  const receiptId = $derived(router.match('/receipt/:id')?.id)
  const isHistory = $derived(router.path === '/history')
  const isSettings = $derived(router.path === '/settings')
</script>

{#if !terminal.paired}
  <PairingScreen />
{:else if payId}
  {#key payId}
    <PayScreen id={payId} />
  {/key}
{:else if receiptId}
  {#key receiptId}
    <ReceiptScreen id={receiptId} />
  {/key}
{:else if isHistory}
  <HistoryScreen />
{:else if isSettings}
  <SettingsScreen />
{:else}
  <KeypadScreen />
{/if}
