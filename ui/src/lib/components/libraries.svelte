<script lang="ts">
	// The Libraries page: each account's libraries on this computer. A
	// sign-in that just happened starts with its first step.
	import FirstStep from '#lib/components/first-step.svelte';
	import LibraryList from '#lib/components/library-list.svelte';
	import { app } from '#lib/app.svelte.js';
	import { m } from '#lib/paraglide/messages.js';

	let { signedIn, ondone }: { signedIn: number | null; ondone: () => void } = $props();

	const status = $derived(app.status);
</script>

<div class="grid gap-6">
	<h1 class="text-lg font-semibold">{m.nav_libraries()}</h1>
	{#each status?.accounts ?? [] as account (account.id)}
		{#if signedIn === account.id}
			<FirstStep {account} {ondone} />
		{:else}
			<LibraryList
				{account}
				pairs={(status?.pairs ?? []).filter((p) => p.account === account.id)}
				offered={app.offered[account.id] ?? []}
			/>
		{/if}
	{/each}
</div>
