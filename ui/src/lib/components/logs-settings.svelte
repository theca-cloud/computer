<script lang="ts">
	// The logs, one set for the whole computer: their level, how many days
	// and how much they keep, their folder. Applied at the next start.
	import { onMount } from 'svelte';
	import { invoke } from '@tauri-apps/api/core';
	import { open } from '@tauri-apps/plugin-dialog';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import { attempt, say } from '#lib/app.svelte.js';
	import { m } from '#lib/paraglide/messages.js';

	type Logs = {
		level: string;
		days: number;
		max_size: number;
		folder: string | null;
		default_folder: boolean;
	};

	const LEVELS = ['error', 'warn', 'info', 'debug', 'trace'];
	const MB = 1_000_000;
	let level = $state('info');
	let days = $state(14);
	let mb = $state(100);
	let folder = $state<string | null>(null);
	let isDefault = $state(true);
	/** The folder picked, saved with the rest; null: the default. */
	let picked = $state<string | null>(null);

	async function load() {
		const logs = await attempt(() => invoke<Logs>('logs'));
		if (!logs) return;
		level = logs.level;
		days = logs.days;
		mb = Math.round(logs.max_size / MB);
		folder = logs.folder;
		isDefault = logs.default_folder;
		picked = logs.default_folder ? null : logs.folder;
	}

	onMount(() => void load());

	async function choose() {
		const chosen = await attempt(() => open({ directory: true, defaultPath: folder ?? undefined }));
		if (typeof chosen === 'string') {
			picked = chosen;
			folder = chosen;
			isDefault = false;
		}
	}

	function useDefault() {
		picked = null;
		isDefault = true;
	}

	async function save() {
		if (!(days >= 1) || !(mb >= 1)) return;
		const saved = await attempt(() =>
			invoke('set_logs', {
				level,
				days: Math.round(days),
				maxSize: Math.round(mb * MB),
				folder: picked
			})
		);
		if (saved !== undefined) say(m.logs_saved());
		await load();
	}
</script>

<div id="logs-settings" class="grid gap-2">
	<h3 class="font-medium">{m.logs()}</h3>
	<div class="flex flex-wrap items-center gap-2">
		<Label for="log-level">{m.logs_level()}</Label>
		<select
			id="log-level"
			class="h-8 rounded-md border bg-background px-2 text-sm"
			bind:value={level}
		>
			{#each LEVELS as l (l)}
				<option value={l}>{l}</option>
			{/each}
		</select>
	</div>
	<div class="flex flex-wrap items-center gap-2">
		<Label for="log-days">{m.logs_days()}</Label>
		<Input id="log-days" type="number" min="1" max="3650" step="1" class="w-20" bind:value={days} />
		<Label for="log-size">{m.logs_size()}</Label>
		<Input id="log-size" type="number" min="1" step="1" class="w-24" bind:value={mb} />
		<span class="text-muted-foreground">MB</span>
	</div>
	<div class="grid gap-1">
		<p class="truncate text-xs text-muted-foreground" title={folder ?? ''}>
			{folder ?? m.logs_none()}{isDefault ? ` (${m.logs_default()})` : ''}
		</p>
		<div class="flex flex-wrap gap-2">
			<Button size="sm" variant="ghost" onclick={choose}>{m.logs_choose()}</Button>
			{#if !isDefault}
				<Button size="sm" variant="ghost" onclick={useDefault}>{m.logs_use_default()}</Button>
			{/if}
			<Button size="sm" variant="ghost" onclick={() => attempt(() => invoke('open_logs'))}>
				{m.logs_open()}
			</Button>
			<Button size="sm" variant="outline" onclick={save}>{m.logs_save()}</Button>
		</div>
	</div>
	<p class="text-xs text-muted-foreground">{m.logs_hint()}</p>
</div>
