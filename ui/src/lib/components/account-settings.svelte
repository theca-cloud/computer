<script lang="ts">
	// An account's settings: its default folder (where new libraries go) and
	// its auto-sync limit (new libraries under it sync by themselves).
	import { invoke } from '@tauri-apps/api/core';
	import { open } from '@tauri-apps/plugin-dialog';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import { Switch } from '#lib/components/ui/switch/index.js';
	import { attempt, type Account } from '#lib/app.svelte.js';
	import { m } from '#lib/paraglide/messages.js';

	let { account }: { account: Account } = $props();

	// The engine's unit (ops::AUTO_SYNC_LIMIT is 500 MiB), shown as "MB".
	const MB = 1024 * 1024;
	let mb = $derived(
		account.auto_sync_limit === null ? 500 : Math.round(account.auto_sync_limit / MB)
	);

	function save(defaultFolder: string, autoSyncLimit: number | null) {
		return attempt(() =>
			invoke('set_account', { account: account.id, defaultFolder, autoSyncLimit })
		);
	}

	async function pick() {
		const picked = await attempt(() =>
			open({ directory: true, defaultPath: account.default_folder })
		);
		if (typeof picked === 'string') await save(picked, account.auto_sync_limit);
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
	<div class="flex flex-wrap items-center gap-3">
		<div class="flex items-center gap-2">
			<Switch
				id="limit-on-{account.id}"
				checked={account.auto_sync_limit !== null}
				onCheckedChange={(on) => save(account.default_folder, on ? mb * MB : null)}
			/>
			<Label for="limit-on-{account.id}">{m.auto_sync()}</Label>
		</div>
		{#if account.auto_sync_limit !== null}
			<div class="flex items-center gap-2">
				<Label for="limit-{account.id}" class="sr-only">{m.auto_sync_limit()}</Label>
				<Input
					id="limit-{account.id}"
					type="number"
					min="1"
					class="w-24"
					bind:value={mb}
					onchange={() => mb >= 1 && save(account.default_folder, Math.round(mb) * MB)}
				/>
				<span class="text-sm text-muted-foreground">{m.auto_sync_unit()}</span>
			</div>
		{/if}
	</div>
</div>
