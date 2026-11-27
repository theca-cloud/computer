<script lang="ts">
	// An account's libraries on this computer, one row each; what it could
	// add is a dialog away, and the libraries the server offers show as a
	// banner above.
	import { onMount } from 'svelte';
	import { invoke } from '@tauri-apps/api/core';
	import AddLibrary from '#lib/components/add-library.svelte';
	import LibraryRow from '#lib/components/library-row.svelte';
	import SignedOut from '#lib/components/signed-out.svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import { attempt, type Account, type Offer, type Pair } from '#lib/app.svelte.js';
	import { m } from '#lib/paraglide/messages.js';

	let {
		account,
		pairs,
		offered
	}: {
		account: Account;
		pairs: Pair[];
		offered: Offer[];
	} = $props();

	const synced = $derived(new Set(pairs.map((p) => p.library)));
	const offers = $derived(offered.filter((o) => !synced.has(o.library)));
	let adding = $state(false);
	/** Why this computer can't hold a drive (the Drive tick greyed), if it can't. */
	let unavailable = $state<string | null>(null);

	onMount(() => {
		void attempt(() => invoke<string | null>('drive_support')).then(
			(why) => (unavailable = why ?? null)
		);
	});
</script>

<section class="grid gap-3" aria-label={m.account_libraries({ email: account.email })}>
	<div class="flex items-center justify-between gap-2">
		<div class="min-w-0">
			<h2 class="truncate text-sm font-medium">{account.email}</h2>
			<p class="truncate text-xs text-muted-foreground">{account.instance}</p>
		</div>
		{#if !account.signed_out}
			<Button size="sm" variant="outline" onclick={() => (adding = true)}>
				{m.libraries_add()}
			</Button>
		{/if}
	</div>

	{#if account.signed_out}<SignedOut {account} />{/if}

	{#if offers.length}
		<div
			class="flex flex-wrap items-center justify-between gap-2 rounded-xl border border-dashed p-3"
			data-offered
		>
			<div class="min-w-0">
				<p class="font-medium">
					{m.offered_banner({ names: offers.map((o) => o.name).join(', ') })}
				</p>
				<p class="text-xs text-muted-foreground">{m.offered_hint()}</p>
			</div>
			<Button size="sm" onclick={() => (adding = true)}>{m.offered_add()}</Button>
		</div>
	{/if}

	{#if pairs.length}
		<ul class="grid gap-2" aria-label={m.synced_folders()}>
			{#each pairs as pair (pair.id)}
				<LibraryRow {pair} />
			{/each}
		</ul>
	{:else}
		<p class="text-muted-foreground">{m.no_pairs()}</p>
	{/if}
</section>

<AddLibrary {account} {synced} bind:open={adding} {unavailable} />
