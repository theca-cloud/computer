<script lang="ts">
	// The Accounts page: each account with its settings, signed in again or
	// removed, and the way to add another.
	import AccountSettings from '#lib/components/account-settings.svelte';
	import Login from '#lib/components/login.svelte';
	import SignedOut from '#lib/components/signed-out.svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { removeAccount } from '#lib/account-actions.js';
	import { app, type SignedIn } from '#lib/app.svelte.js';
	import { m } from '#lib/paraglide/messages.js';

	let { onaccount }: { onaccount: (signedIn: SignedIn) => void } = $props();

	let adding = $state(false);
</script>

<div class="grid gap-4">
	<h1 class="text-lg font-semibold">{m.nav_accounts()}</h1>
	{#each app.status?.accounts ?? [] as account (account.id)}
		<Card.Root>
			<Card.Content class="grid gap-4">
				{#if account.signed_out}<SignedOut {account} />{/if}
				<AccountSettings {account} />
			</Card.Content>
			{#if !account.signed_out}
				<Card.Footer>
					<Button size="sm" variant="ghost" class="ml-auto" onclick={() => removeAccount(account)}>
						{m.remove_account()}
					</Button>
				</Card.Footer>
			{/if}
		</Card.Root>
	{/each}
	{#if adding}
		<Card.Root>
			<Card.Header><Card.Title>{m.add_account()}</Card.Title></Card.Header>
			<Card.Content>
				<Login
					onaccount={(signedIn) => {
						adding = false;
						onaccount(signedIn);
					}}
					oncancel={() => (adding = false)}
				/>
			</Card.Content>
		</Card.Root>
	{:else}
		<div>
			<Button variant="outline" onclick={() => (adding = true)}>{m.add_account()}</Button>
		</div>
	{/if}
</div>
