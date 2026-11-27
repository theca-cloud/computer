<script lang="ts">
	// The window: a sidebar of pages, one per topic (libraries, activity,
	// accounts, settings). The first launch signs an account in; then the
	// page shown is the one the sidebar says.
	import AccountsPage from '#lib/components/accounts.svelte';
	import ActivityPage from '#lib/components/activity.svelte';
	import AskDialog from '#lib/components/ask-dialog.svelte';
	import LibrariesPage from '#lib/components/libraries.svelte';
	import Login from '#lib/components/login.svelte';
	import SettingsPage from '#lib/components/settings-page.svelte';
	import Sidebar from '#lib/components/sidebar.svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { app, attempt, follow, go, nav, reload, type SignedIn } from '#lib/app.svelte.js';
	import { m } from '#lib/paraglide/messages.js';

	$effect(follow);

	/** Just signed in: its first step shows, until Add or Later. */
	let signedIn = $state<number | null>(null);

	const status = $derived(app.status);

	function onaccount({ account, fresh }: SignedIn) {
		// Signed in again: its choices stand, no first step.
		signedIn = fresh ? account : null;
		// The libraries are where a new account starts.
		if (fresh) go('libraries');
		void attempt(reload);
	}
</script>

{#if !status}
	<main class="mx-auto grid max-w-2xl gap-8 p-4 text-sm sm:p-6">
		<p class="text-muted-foreground" role="status">{m.loading()}</p>
	</main>
{:else if status.accounts.length === 0}
	<main class="mx-auto grid max-w-2xl gap-8 p-4 text-sm sm:p-6">
		<Card.Root>
			<Card.Header>
				<Card.Title class="text-lg">{m.welcome_title()}</Card.Title>
				<Card.Description>{m.welcome_text()}</Card.Description>
			</Card.Header>
			<Card.Content>
				<Login {onaccount} />
			</Card.Content>
		</Card.Root>
	</main>
{:else}
	<div class="flex h-screen text-sm">
		<Sidebar />
		<main class="min-w-0 flex-1 overflow-y-auto p-4 sm:p-6">
			<div class="mx-auto max-w-2xl">
				{#if nav.page === 'libraries'}
					<LibrariesPage {signedIn} ondone={() => (signedIn = null)} />
				{:else if nav.page === 'activity'}
					<ActivityPage />
				{:else if nav.page === 'accounts'}
					<AccountsPage {onaccount} />
				{:else}
					<SettingsPage />
				{/if}
			</div>
		</main>
	</div>
{/if}

{#if app.message}
	<div
		class={[
			'fixed inset-x-4 bottom-4 mx-auto flex max-w-2xl items-start gap-2 rounded-lg bg-popover p-3 text-sm shadow-lg ring-1',
			app.message.error ? 'text-destructive ring-destructive/30' : 'ring-foreground/10'
		]}
		role={app.message.error ? 'alert' : 'status'}
	>
		<p class="min-w-0 flex-1 break-words">{app.message.text}</p>
		<Button size="xs" variant="ghost" onclick={() => (app.message = null)}>{m.dismiss()}</Button>
	</div>
{/if}

<AskDialog />
