<script lang="ts">
	// A drive's part of its line: its numbers, the macOS switch-on step, and
	// what it keeps on this computer, with Keep… (a picker in the drive) and
	// Free.
	import { invoke } from '@tauri-apps/api/core';
	import { open } from '@tauri-apps/plugin-dialog';
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { Button } from '#lib/components/ui/button/index.js';
	import { attempt, say, type Pair } from '#lib/app.svelte.js';
	import { formatSize } from '#lib/format.js';
	import { m } from '#lib/paraglide/messages.js';

	let { pair }: { pair: Pair } = $props();

	const drive = $derived(pair.drive!);
	/** What it keeps, by path below its folder; loaded when shown. */
	let kept = $state<string[] | null>(null);

	/** System Settings at its File Providers pane. */
	const SETTINGS = 'x-apple.systempreferences:com.apple.LoginItems-Settings.extension';

	const join = (rel: string) => `${pair.path}/${rel}`;

	async function load() {
		kept = (await attempt(() => invoke<string[]>('kept', { pair: pair.id }))) ?? [];
	}

	async function keep() {
		const picked = await attempt(() =>
			open({ defaultPath: pair.path, multiple: true, directory: false })
		);
		const paths = Array.isArray(picked) ? picked : typeof picked === 'string' ? [picked] : [];
		for (const path of paths) await attempt(() => invoke('keep', { path }));
		if (paths.length) await load();
	}

	async function free(rel: string) {
		const done = await attempt(() => invoke('free', { path: join(rel) }));
		if (done !== undefined) say(m.drive_freed({ path: rel }));
		await load();
	}
</script>

<div class="grid gap-1">
	<p class="text-xs text-muted-foreground">
		{drive.downloaded !== null && drive.waiting !== null
			? m.drive_numbers({
					downloaded: formatSize(drive.downloaded),
					kept: drive.kept,
					waiting: drive.waiting
				})
			: m.drive_kept_only({ kept: drive.kept })}
	</p>
	{#if drive.switch_on}
		<div class="grid gap-2 rounded-lg bg-secondary p-3" role="status">
			<p>{m.drive_switch_on()}</p>
			<div>
				<Button size="sm" onclick={() => attempt(() => openUrl(SETTINGS))}>
					{m.drive_open_settings()}
				</Button>
			</div>
		</div>
	{/if}
	<div class="flex flex-wrap gap-1">
		<Button
			size="sm"
			variant="ghost"
			aria-expanded={kept !== null}
			onclick={() => (kept === null ? load() : (kept = null))}
		>
			{kept === null ? m.drive_kept_show() : m.drive_kept_hide()}
		</Button>
		<Button
			size="sm"
			variant="ghost"
			aria-label="{m.drive_keep()} {pair.library_name}"
			onclick={keep}
		>
			{m.drive_keep()}
		</Button>
	</div>
	{#if kept !== null}
		<div class="grid gap-1">
			<h4 class="text-xs font-medium">{m.drive_kept_title()}</h4>
			{#if kept.length}
				<ul class="grid gap-1">
					{#each kept as rel (rel)}
						<li class="flex items-center justify-between gap-2">
							<span class="min-w-0 truncate font-mono text-xs" title={rel}>{rel}</span>
							<Button
								size="xs"
								variant="ghost"
								aria-label="{m.drive_free()} {rel}"
								onclick={() => free(rel)}
							>
								{m.drive_free()}
							</Button>
						</li>
					{/each}
				</ul>
			{:else}
				<p class="text-xs text-muted-foreground">{m.drive_kept_none()}</p>
			{/if}
		</div>
	{/if}
</div>
