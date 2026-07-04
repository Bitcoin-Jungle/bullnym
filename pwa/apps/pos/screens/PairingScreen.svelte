<script lang="ts">
  // Full-screen gate rendered by apps/pos/App.svelte whenever the terminal
  // is unpaired (see lib/stores/terminal.svelte.ts). Shows the pairing code
  // as text plus a client-rendered QR of the wallet deep link (QrCard.svelte
  // — the server's /qr.svg endpoint is deliberately NOT used here, per
  // .context/pos-terminal-plan.md: the deep link carries pairing_id+code+nym,
  // which the server-side QR endpoint has no way to encode). The deep link
  // is also a tappable anchor for the same-device path (merchant opens the
  // wallet app on the same device the POS terminal is running on).
  //
  // The terminal store owns the actual pairing state machine (startPairing/
  // poll/unpair) — this component only renders it and drives the countdown
  // display off `expiresAtUnix`.
  import { config } from '$lib/config'
  import { terminal } from '$lib/stores/terminal.svelte'
  import QrCard from '$lib/components/QrCard.svelte'
  import Button from '$lib/components/Button.svelte'
  import BullSpinner from '$lib/components/BullSpinner.svelte'
  import BullFooter from '$lib/components/BullFooter.svelte'

  void terminal.startPairing()

  let nowUnix = $state(Math.floor(Date.now() / 1000))
  $effect(() => {
    const t = setInterval(() => {
      nowUnix = Math.floor(Date.now() / 1000)
    }, 1000)
    return () => clearInterval(t)
  })

  const pairing = $derived(terminal.pairing)

  const deepLink = $derived(
    pairing.kind === 'pairing'
      ? `bullbitcoin://bullnym/pos-pair?pairing_id=${encodeURIComponent(pairing.pairingId)}&code=${encodeURIComponent(pairing.code)}&nym=${encodeURIComponent(config.nym)}`
      : '',
  )

  const secondsLeft = $derived(pairing.kind === 'pairing' ? Math.max(0, pairing.expiresAtUnix - nowUnix) : 0)

  function retry() {
    void terminal.startPairing()
  }
</script>

<main class="grid min-h-[100dvh] place-items-center bg-[#f5f0e8] px-5 py-8 text-[#211f1a] dark:bg-[#161512] dark:text-[#fff6e8]">
  <div class="mx-auto flex w-full max-w-sm flex-col items-center gap-5 text-center">
    <div>
      <h1 class="font-display text-3xl uppercase tracking-display leading-none">{config.header || config.nym}</h1>
      <p class="mt-2 text-sm text-[#776b5a] dark:text-[#b9aa91]">
        Pair this terminal with your Bull Bitcoin wallet to start taking payments.
      </p>
    </div>

    {#if pairing.kind === 'pairing'}
      <p class="font-display text-6xl uppercase tracking-display leading-none" aria-label="Pairing code">
        {pairing.code}
      </p>
      <QrCard value={deepLink} label="Pairing code" />
      <a class="text-sm font-semibold underline underline-offset-4" href={deepLink}>
        Open in Bull Bitcoin wallet
      </a>
      <p class="text-xs text-[#776b5a] dark:text-[#b9aa91]">
        {secondsLeft > 0 ? `Code expires in ${secondsLeft}s` : 'Code expiring…'}
      </p>
      <p class="flex items-center gap-2 text-sm font-semibold">
        <span class="h-2 w-2 animate-pulse rounded-full bg-[#1e4e73]"></span>
        Waiting for approval…
      </p>
    {:else if pairing.kind === 'expired'}
      <p class="font-display text-3xl uppercase tracking-display leading-none text-[#8c2d28]">Code expired</p>
      <p class="text-sm text-[#776b5a] dark:text-[#b9aa91]">Get a new code to keep pairing.</p>
      <Button onclick={retry}>Get new code</Button>
    {:else if pairing.kind === 'error'}
      <p class="rounded-md bg-[#ffe0d9] px-4 py-3 text-sm font-semibold text-[#8c2d28]">{pairing.message}</p>
      <Button onclick={retry}>Try again</Button>
    {:else}
      <BullSpinner size={56} label="Preparing pairing code" />
    {/if}

    <BullFooter />
  </div>
</main>
