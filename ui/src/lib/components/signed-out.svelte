<script lang="ts">
	// An account the server refused: sign in again (the browser flow), or
	// remove it.
	import LoginCode from '#lib/components/login-code.svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import { removeAccount } from '#lib/account-actions.js';
	import { attempt, say, signIn, type Account } from '#lib/app.svelte.js';
	import { m } from '#lib/paraglide/messages.js';

	let { account }: { account: Account } = $props();

	let code = $state('');
	let attempts = 0;

	async function signInAgain() {
		const started = await attempt(() => signIn(account.instance, account.device, null));
		if (!started) return;
		code = started.code;
		const mine = ++attempts;
		started.account.then(
			() => mine === attempts && (code = ''),
			(error) => {
				if (mine !== attempts) return;
				code = '';
				say(String(error), true);
			}
		);
	}
</script>

<div class="grid gap-2 rounded-lg bg-destructive/10 p-3" role="alert">
	<p>{m.signed_out({ instance: account.instance })}</p>
	{#if code}
		<LoginCode {code} oncancel={() => (attempts++, (code = ''))} />
	{:else}
		<div class="flex flex-wrap gap-2">
			<Button size="sm" onclick={signInAgain}>{m.sign_in_again()}</Button>
			<Button size="sm" variant="destructive" onclick={() => removeAccount(account)}>
				{m.remove_account()}
			</Button>
		</div>
	{/if}
</div>
