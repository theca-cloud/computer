<script lang="ts">
	// This version; where the app updates itself (the `self-update` build),
	// how (auto, ask, off), Check for updates, and the release waiting.
	import { onMount } from 'svelte';
	import { invoke } from '@tauri-apps/api/core';
	import { listen } from '@tauri-apps/api/event';
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import { attempt } from '#lib/app.svelte.js';
	import { m } from '#lib/paraglide/messages.js';

	type Offered = { version: string; notes: string; why: string | null };
	type Updates = {
		version: string;
		self_update: boolean;
		mode: string | null;
		offered: Offered | null;
	};

	let updates = $state<Updates | null>(null);
	/** Checked from here, nothing newer: said. */
	let latest = $state(false);
	let busy = $state(false);

	async function load() {
		updates = (await attempt(() => invoke<Updates>('updates'))) ?? updates;
	}

	onMount(() => {
		void load();
		const unlisten = listen<Updates>('updates', ({ payload }) => {
			updates = payload;
		});
		return () => void unlisten.then((stop) => stop());
	});

	async function setMode(mode: string) {
		await attempt(() => invoke('set_updates', { mode }));
		await load();
	}

	async function check() {
		busy = true;
		const checked = await attempt(() => invoke<Updates>('check_updates'));
		busy = false;
		if (checked) {
			updates = checked;
			latest = checked.offered === null;
		}
	}

	async function install() {
		busy = true;
		await attempt(() => invoke('install_update'));
		busy = false;
	}
</script>

<div id="updates-settings" class="grid gap-2">
	<h3 class="font-medium">{m.updates()}</h3>
	<p class="text-sm">{m.updates_version({ version: updates?.version ?? '' })}</p>
	{#if updates?.self_update}
		<div class="flex flex-wrap items-center gap-2">
			<Label for="updates-mode">{m.updates_mode()}</Label>
			<select
				id="updates-mode"
				class="h-8 rounded-md border bg-background px-2 text-sm"
				value={updates.mode}
				onchange={(e) => setMode(e.currentTarget.value)}
			>
				<option value="auto">{m.updates_auto()}</option>
				<option value="ask">{m.updates_ask()}</option>
				<option value="off">{m.updates_off()}</option>
			</select>
			<Button size="sm" variant="outline" disabled={busy} onclick={check}>
				{m.updates_check()}
			</Button>
		</div>
		{#if updates.offered}
			{@const offered = updates.offered}
			<div class="flex flex-wrap items-center gap-2">
				<span class="text-sm">{m.updates_available({ version: offered.version })}</span>
				<Button size="sm" variant="ghost" onclick={() => attempt(() => openUrl(offered.notes))}>
					{m.updates_notes()}
				</Button>
				<Button size="sm" disabled={busy} onclick={install}>{m.updates_install()}</Button>
			</div>
			{#if offered.why}
				<p class="text-xs text-muted-foreground">{offered.why}</p>
			{/if}
		{:else if latest}
			<p class="text-xs text-muted-foreground">{m.updates_latest()}</p>
		{/if}
	{/if}
</div>
