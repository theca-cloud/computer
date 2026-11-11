<script lang="ts">
	// The drive limit, one for the whole computer: how much downloaded
	// content the drives keep (kept files and edits not uploaded aside).
	import { onMount } from 'svelte';
	import { invoke } from '@tauri-apps/api/core';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import { attempt } from '#lib/app.svelte.js';
	import { m } from '#lib/paraglide/messages.js';

	const GIB = 1024 ** 3;
	let gib = $state<number | null>(null);
	let isDefault = $state(true);

	async function load() {
		const limit = await attempt(() => invoke<{ bytes: number; default: boolean }>('drive_limit'));
		if (!limit) return;
		gib = Math.round((limit.bytes / GIB) * 10) / 10;
		isDefault = limit.default;
	}

	onMount(() => void load());

	async function save() {
		if (gib === null || !(gib > 0)) return;
		const bytes = Math.round(gib * GIB);
		await attempt(() => invoke('set_drive_limit', { bytes }));
		await load();
	}
</script>

<div class="grid gap-1.5">
	<Label for="drive-limit">{m.drive_limit()}</Label>
	<div class="flex items-center gap-2">
		<Input id="drive-limit" type="number" min="0.1" step="0.1" class="w-28" bind:value={gib} />
		<span class="text-muted-foreground">{m.drive_limit_unit()}</span>
		<Button size="sm" variant="outline" onclick={save}>{m.drive_limit_save()}</Button>
	</div>
	<p class="text-xs text-muted-foreground">
		{m.drive_limit_hint()}{isDefault ? ` (${m.drive_limit_default()})` : ''}
	</p>
</div>
