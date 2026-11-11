<script lang="ts">
	// An account's settings: its default folder (where new libraries go),
	// what libraries new to it do, and the size up to which they sync.
	import { invoke } from '@tauri-apps/api/core';
	import { open } from '@tauri-apps/plugin-dialog';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import { attempt, type Account, type NewLibraries } from '#lib/app.svelte.js';
	import { m } from '#lib/paraglide/messages.js';

	let { account }: { account: Account } = $props();

	// The engine's unit (ops::AUTO_SYNC_LIMIT is 500 MiB), shown as "MB".
	const MB = 1024 * 1024;
	let mb = $derived(Math.round(account.auto_sync_limit / MB));

	function save(defaultFolder: string, newLibraries: NewLibraries, autoSyncLimit: number) {
		return attempt(() =>
			invoke('set_account', { account: account.id, defaultFolder, newLibraries, autoSyncLimit })
		);
	}

	async function pick() {
		const picked = await attempt(() =>
			open({ directory: true, defaultPath: account.default_folder })
		);
		if (typeof picked === 'string')
			await save(picked, account.new_libraries, account.auto_sync_limit);
	}
</script>

<div class="grid gap-3">
	<h3 class="truncate font-medium">{account.email} · {account.instance}</h3>
	<div class="grid gap-1.5">
		<span class="text-sm">{m.default_folder()}</span>
		<div class="flex items-center gap-2">
			<p class="min-w-0 flex-1 truncate text-muted-foreground" title={account.default_folder}>
				{account.default_folder}
			</p>
			<Button
				size="sm"
				variant="outline"
				aria-label="{m.change()} {m.default_folder()}"
				onclick={pick}
			>
				{m.change()}
			</Button>
		</div>
	</div>
	<div class="flex flex-wrap items-center gap-2">
		<Label for="new-libraries-{account.id}">{m.first_new()}</Label>
		<select
			id="new-libraries-{account.id}"
			class="h-8 rounded-md border bg-background px-2 text-sm"
			value={account.new_libraries}
			onchange={(e) =>
				save(
					account.default_folder,
					e.currentTarget.value as NewLibraries,
					account.auto_sync_limit
				)}
		>
			<option value="notify">{m.new_notify()}</option>
			<option value="silent">{m.new_silent()}</option>
			<option value="sync">{m.new_sync()}</option>
			<option value="drive">{m.new_drive()}</option>
		</select>
		{#if account.new_libraries === 'sync'}
			<Label for="limit-{account.id}" class="text-xs">{m.new_limit()}</Label>
			<Input
				id="limit-{account.id}"
				type="number"
				min="0"
				class="w-24"
				bind:value={mb}
				onchange={() =>
					typeof mb === 'number' &&
					mb >= 0 &&
					save(account.default_folder, 'sync', Math.round(mb) * MB)}
			/>
			<span class="text-sm text-muted-foreground">{m.auto_sync_unit()}</span>
		{/if}
	</div>
	{#if account.new_libraries === 'sync'}
		<p class="text-xs text-muted-foreground">{m.new_limit_hint()}</p>
	{/if}
</div>
