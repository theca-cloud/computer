<script lang="ts">
	// The window: one page in sections (roadmap 09, GUI). The first launch
	// signs an account in; then its accounts and synced folders, the notices,
	// the settings.
	import { onMount } from 'svelte';
	import { invoke } from '@tauri-apps/api/core';
	import { disable, enable, isEnabled } from '@tauri-apps/plugin-autostart';
	import AccountCard from '#lib/components/account.svelte';
	import AccountSettings from '#lib/components/account-settings.svelte';
	import AskDialog from '#lib/components/ask-dialog.svelte';
	import DriveSettings from '#lib/components/drive-settings.svelte';
	import FirstStep from '#lib/components/first-step.svelte';
	import LogsSettings from '#lib/components/logs-settings.svelte';
	import UpdatesSettings from '#lib/components/updates-settings.svelte';
	import Login from '#lib/components/login.svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import { Separator } from '#lib/components/ui/separator/index.js';
	import { Switch } from '#lib/components/ui/switch/index.js';
	import { app, attempt, follow, reload, say, type SignedIn } from '#lib/app.svelte.js';
	import { formatDate } from '#lib/format.js';
	import { m } from '#lib/paraglide/messages.js';

	$effect(follow);

	/** Just signed in: its first step shows, until Add or Later. */
	let signedIn = $state<number | null>(null);
	let adding = $state(false);
	let autostart = $state(false);

	onMount(() => {
		void attempt(isEnabled).then((on) => (autostart = on ?? false));
	});

	const status = $derived(app.status);
	const pairs = $derived(status?.pairs ?? []);
	const allPaused = $derived(pairs.length > 0 && pairs.every((p) => p.paused));
	// Errors first, newest first.
	const notices = $derived(
		[...(status?.notices ?? [])].sort(
			(a, b) =>
				Number(b.level === 'error') - Number(a.level === 'error') || b.at.localeCompare(a.at)
		)
	);
	const libraryOf = (pair: number | null) => pairs.find((p) => p.id === pair)?.library_name;

	function onaccount({ account, fresh }: SignedIn) {
		// Signed in again: its choices stand, no first step.
		signedIn = fresh ? account : null;
		adding = false;
		void attempt(reload);
	}

	async function undo(notice: number) {
		const restored = await attempt(() => invoke<number>('undo', { notice }));
		if (restored !== undefined) say(m.restored({ count: restored }));
	}

	async function setAutostart(on: boolean) {
		await attempt(() => (on ? enable() : disable()));
		autostart = (await attempt(isEnabled)) ?? false;
	}
</script>

{#snippet heading(title: string)}
	<h2 class="text-base font-medium">{title}</h2>
{/snippet}

<main class="mx-auto grid max-w-2xl gap-8 p-4 text-sm sm:p-6">
	{#if !status}
		<p class="text-muted-foreground" role="status">{m.loading()}</p>
	{:else if status.accounts.length === 0}
		<Card.Root>
			<Card.Header>
				<Card.Title class="text-lg">{m.welcome_title()}</Card.Title>
				<Card.Description>{m.welcome_text()}</Card.Description>
			</Card.Header>
			<Card.Content>
				<Login {onaccount} />
			</Card.Content>
		</Card.Root>
	{:else}
		<header class="flex items-center justify-between gap-2">
			<h1 class="text-lg font-medium">{m.app_name()}</h1>
			{#if pairs.length}
				<Button
					size="sm"
					variant="outline"
					onclick={() => attempt(() => invoke('set_paused', { pair: null, paused: !allPaused }))}
				>
					{allPaused ? m.resume_all() : m.pause_all()}
				</Button>
			{/if}
		</header>

		<section class="grid gap-3" aria-labelledby="accounts">
			<h2 id="accounts" class="text-base font-medium">{m.accounts()}</h2>
			{#each status.accounts as account (account.id)}
				{#if signedIn === account.id}
					<FirstStep {account} ondone={() => (signedIn = null)} />
				{:else}
					<AccountCard
						{account}
						pairs={pairs.filter((p) => p.account === account.id)}
						offered={app.offered[account.id] ?? []}
					/>
				{/if}
			{/each}
			{#if adding}
				<Card.Root>
					<Card.Header><Card.Title>{m.add_account()}</Card.Title></Card.Header>
					<Card.Content>
						<Login {onaccount} oncancel={() => (adding = false)} />
					</Card.Content>
				</Card.Root>
			{:else}
				<div>
					<Button variant="outline" onclick={() => (adding = true)}>{m.add_account()}</Button>
				</div>
			{/if}
		</section>

		{#if notices.length}
			<section class="grid gap-3">
				{@render heading(m.notices())}
				<ul class="grid gap-2">
					{#each notices as notice (notice.id)}
						{@const library = libraryOf(notice.pair)}
						<li
							class={[
								'grid gap-1 rounded-lg p-3 ring-1',
								notice.level === 'error'
									? 'bg-destructive/10 ring-destructive/20'
									: 'ring-foreground/10'
							]}
						>
							<p>{notice.message}</p>
							{#if notice.path}
								<p class="truncate font-mono text-xs" title={notice.path}>
									{library ? `${library}: ` : ''}{notice.path}
								</p>
							{/if}
							<div class="flex flex-wrap items-center justify-between gap-2">
								<time class="text-xs text-muted-foreground" datetime={notice.at}>
									{formatDate(notice.at)}
								</time>
								<div class="flex gap-1">
									{#if notice.undo}
										<Button size="sm" variant="outline" onclick={() => undo(notice.id)}>
											{m.undo()}
										</Button>
									{/if}
									<Button
										size="sm"
										variant="ghost"
										onclick={() => attempt(() => invoke('dismiss', { notice: notice.id }))}
									>
										{m.dismiss()}
									</Button>
								</div>
							</div>
						</li>
					{/each}
				</ul>
			</section>
		{/if}

		<section class="grid gap-4">
			{@render heading(m.settings())}
			{#each status.accounts as account (account.id)}
				<AccountSettings {account} />
				<Separator />
			{/each}
			<DriveSettings />
			<Separator />
			<LogsSettings />
			<Separator />
			<UpdatesSettings />
			<Separator />
			<div class="flex items-center gap-2">
				<Switch id="autostart" checked={autostart} onCheckedChange={setAutostart} />
				<Label for="autostart">{m.autostart()}</Label>
			</div>
			<div>
				<Button variant="destructive" onclick={() => attempt(() => invoke('quit'))}>
					{m.quit()}
				</Button>
			</div>
		</section>
	{/if}
</main>

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
