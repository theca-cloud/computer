<script lang="ts">
	// A library on this computer: a synced folder or a drive. Its line says
	// what it is, where, and how it stands; a click opens its details, and
	// its actions are in the menu beside it.
	import { invoke } from '@tauri-apps/api/core';
	import { open } from '@tauri-apps/plugin-dialog';
	import CloudIcon from '@lucide/svelte/icons/cloud';
	import EllipsisIcon from '@lucide/svelte/icons/ellipsis';
	import FolderIcon from '@lucide/svelte/icons/folder';
	import DriveView from '#lib/components/drive.svelte';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js';
	import * as Sheet from '#lib/components/ui/sheet/index.js';
	import { app, ask, attempt, say, type Pair } from '#lib/app.svelte.js';
	import { keepFiles } from '#lib/drive-actions.js';
	import { ago, formatSize } from '#lib/format.js';
	import { m } from '#lib/paraglide/messages.js';
	import { pairStatus } from '#lib/status.js';

	let { pair }: { pair: Pair } = $props();

	let details = $state(false);
	const status = $derived(pairStatus(pair));
	const drive = $derived(pair.drive);

	const setPaused = (paused: boolean) =>
		attempt(() => invoke('set_paused', { pair: pair.id, paused }));

	/** Points a stopped pair at its folder, moved or another one (confirmed). */
	async function locate() {
		const path = await attempt(() => open({ directory: true, defaultPath: pair.path }));
		if (typeof path !== 'string') return;
		const call = (confirm: boolean) =>
			attempt(() => invoke<'moved' | 'different'>('locate', { pair: pair.id, path, confirm }));
		if ((await call(false)) !== 'different') return;
		const sure = await ask(m.locate_title(), m.locate_text({ library: pair.library_name }), [
			{ label: m.locate_confirm(), value: 'sync' }
		]);
		if (sure) await call(true);
	}

	async function remove() {
		const choice = await ask(m.remove_title({ library: pair.library_name }), m.remove_text(), [
			{ label: m.delete_files(), value: 'delete', variant: 'destructive' },
			{ label: m.keep_files(), value: 'keep' }
		]);
		if (!choice) return;
		const kept = await attempt(() =>
			invoke<number>('remove', { pair: pair.id, deleteFiles: choice === 'delete' })
		);
		if (kept) say(m.kept({ count: kept }));
	}

	const openFolder = () => attempt(() => invoke('open_folder', { pair: pair.id }));
</script>

<li
	class="flex items-start gap-1 rounded-xl border bg-card p-2 text-card-foreground shadow-xs"
	data-library={pair.library_name}
>
	<button
		type="button"
		class="flex min-w-0 flex-1 items-start gap-3 rounded-lg p-1 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring"
		aria-label={m.library_more({ library: pair.library_name })}
		onclick={() => (details = true)}
	>
		<span
			class={[
				'flex size-9 shrink-0 items-center justify-center rounded-lg',
				drive
					? 'bg-violet-500/10 text-violet-600 dark:text-violet-400'
					: 'bg-blue-500/10 text-blue-600 dark:text-blue-400'
			]}
			aria-hidden="true"
		>
			{#if drive}
				<CloudIcon class="size-4.5" />
			{:else}
				<FolderIcon class="size-4.5" />
			{/if}
		</span>
		<span class="grid min-w-0 flex-1 gap-0.5">
			<span class="flex flex-wrap items-center gap-x-2 gap-y-0.5">
				<span class="text-sm font-semibold">{pair.library_name}</span>
				<span class="text-xs text-muted-foreground">
					{drive ? m.kind_drive() : m.kind_folder()}
				</span>
				{#if !pair.writable}<Badge variant="outline">{m.read_only()}</Badge>{/if}
			</span>
			<span class="truncate text-xs text-muted-foreground" title={pair.path}>{pair.path}</span>
			<span class="flex items-center gap-1.5 text-xs">
				<span class={['size-1.75 shrink-0 rounded-full', status.dot]}></span>
				<span class="truncate">{status.text}</span>
			</span>
			{#if drive}
				<span class="text-xs text-muted-foreground">
					{drive.downloaded !== null && drive.waiting !== null
						? m.drive_numbers({
								downloaded: formatSize(drive.downloaded),
								kept: drive.kept,
								waiting: drive.waiting
							})
						: m.drive_kept_only({ kept: drive.kept })}
				</span>
			{/if}
		</span>
	</button>
	<DropdownMenu.Root>
		<DropdownMenu.Trigger>
			{#snippet child({ props })}
				<Button
					{...props}
					variant="ghost"
					size="icon-sm"
					aria-label="{m.library_more({ library: pair.library_name })} ⋯"
				>
					<EllipsisIcon aria-hidden="true" />
				</Button>
			{/snippet}
		</DropdownMenu.Trigger>
		<DropdownMenu.Content align="end" class="min-w-44">
			<DropdownMenu.Item onSelect={openFolder}>{m.open_folder()}</DropdownMenu.Item>
			{#if drive}
				<DropdownMenu.Item onSelect={() => keepFiles(pair)}>{m.drive_keep()}</DropdownMenu.Item>
				<DropdownMenu.Item onSelect={() => (details = true)}>
					{m.drive_kept_show()}
				</DropdownMenu.Item>
			{/if}
			<DropdownMenu.Separator />
			{#if !pair.stopped}
				<DropdownMenu.Item onSelect={() => setPaused(!pair.paused)}>
					{pair.paused ? m.resume() : m.pause()}
				</DropdownMenu.Item>
			{:else if pair.folder_stop}
				<DropdownMenu.Item onSelect={() => setPaused(false)}>{m.try_again()}</DropdownMenu.Item>
				<!-- A drive's folder is the system's: it can't move. -->
				{#if !drive}
					<DropdownMenu.Item onSelect={locate}>{m.locate()}</DropdownMenu.Item>
				{/if}
			{/if}
			<DropdownMenu.Item variant="destructive" onSelect={remove}>{m.remove()}</DropdownMenu.Item>
		</DropdownMenu.Content>
	</DropdownMenu.Root>
</li>

<Sheet.Root bind:open={details}>
	<Sheet.Content side="right" class="gap-3 p-4">
		<Sheet.Header class="p-0">
			<Sheet.Title>{m.library_details_title({ library: pair.library_name })}</Sheet.Title>
			<Sheet.Description>{drive ? m.kind_drive() : m.kind_folder()}</Sheet.Description>
		</Sheet.Header>
		<div class="grid gap-3 overflow-y-auto text-sm">
			<div class="grid gap-1">
				<span class="text-xs font-medium text-muted-foreground">{m.library_folder()}</span>
				<p class="break-all">{pair.path}</p>
				<div>
					<Button size="sm" variant="outline" onclick={openFolder}>{m.open_folder()}</Button>
				</div>
			</div>
			<div class="flex items-center gap-1.5">
				<span class={['size-2 shrink-0 rounded-full', status.dot]}></span>
				<span>{status.text}</span>
			</div>
			<p class="text-xs text-muted-foreground">
				{pair.synced_at
					? m.library_last_change({ when: ago(pair.synced_at, app.now) })
					: m.never_synced()}
			</p>
			{#if drive}<DriveView {pair} />{/if}
		</div>
	</Sheet.Content>
</Sheet.Root>
