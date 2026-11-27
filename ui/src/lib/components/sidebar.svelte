<script lang="ts">
	// The window's sections, one page each, the overall state and Pause all.
	// Below 640 px it shows its icons only (their names are the buttons'
	// labels and tooltips).
	import { invoke } from '@tauri-apps/api/core';
	import BellIcon from '@lucide/svelte/icons/bell';
	import FolderIcon from '@lucide/svelte/icons/folder';
	import PauseIcon from '@lucide/svelte/icons/pause';
	import PlayIcon from '@lucide/svelte/icons/play';
	import SettingsIcon from '@lucide/svelte/icons/settings';
	import UserIcon from '@lucide/svelte/icons/user';
	import type { Component } from 'svelte';
	import { app, attempt, go, nav, type Page } from '#lib/app.svelte.js';
	import { m } from '#lib/paraglide/messages.js';

	const pairs = $derived(app.status?.pairs ?? []);
	const errors = $derived((app.status?.notices ?? []).filter((n) => n.level === 'error').length);
	const allPaused = $derived(pairs.length > 0 && pairs.every((p) => p.paused));

	/** What the footer says of every library together, and the dot's colour. */
	const overall = $derived.by(() => {
		const signedOut = (app.status?.accounts ?? []).some((a) => a.signed_out);
		if (!pairs.length) return { text: m.overall_none(), dot: 'bg-muted-foreground' };
		if (signedOut || pairs.some((p) => p.stopped || p.error))
			return { text: m.overall_attention(), dot: 'bg-destructive' };
		if (allPaused) return { text: m.overall_paused(), dot: 'bg-muted-foreground' };
		if (pairs.some((p) => ['syncing', 'listing the library', 'waiting'].includes(p.activity)))
			return { text: m.overall_syncing(), dot: 'bg-blue-500' };
		return { text: m.overall_up_to_date(), dot: 'bg-primary' };
	});

	const items: { page: Page; label: () => string; icon: Component }[] = [
		{ page: 'libraries', label: m.nav_libraries, icon: FolderIcon },
		{ page: 'activity', label: m.nav_activity, icon: BellIcon },
		{ page: 'accounts', label: m.nav_accounts, icon: UserIcon },
		{ page: 'settings', label: m.nav_settings, icon: SettingsIcon }
	];
</script>

<nav
	aria-label={m.nav_label()}
	class="flex w-12 shrink-0 flex-col gap-1 border-r bg-muted/40 p-1.5 sm:w-48 sm:p-2.5"
>
	<div class="flex items-center gap-2 px-2 pt-1 pb-3">
		<div
			class="flex size-6 shrink-0 items-center justify-center rounded-md bg-primary text-sm font-semibold text-primary-foreground"
			aria-hidden="true"
		>
			T
		</div>
		<span class="hidden text-base font-semibold sm:inline">{m.app_name()}</span>
	</div>
	{#each items as item (item.page)}
		{@const Icon = item.icon}
		{@const active = nav.page === item.page}
		<button
			id="nav-{item.page}"
			type="button"
			class={[
				'flex items-center gap-2.5 rounded-lg px-2 py-2 text-left text-sm outline-none focus-visible:ring-2 focus-visible:ring-ring',
				active ? 'bg-primary/10 font-medium text-primary' : 'text-foreground/80 hover:bg-muted'
			]}
			aria-label={item.label()}
			aria-current={active ? 'page' : undefined}
			title={item.label()}
			onclick={() => go(item.page)}
		>
			<Icon class="size-4 shrink-0" aria-hidden="true" />
			<span class="hidden flex-1 sm:inline">{item.label()}</span>
			{#if item.page === 'activity' && errors}
				<span
					class="ml-auto flex min-w-4.5 items-center justify-center rounded-full bg-destructive px-1.5 text-xs text-white"
					title={m.notices_errors({ count: errors })}
				>
					{errors}
				</span>
			{/if}
		</button>
	{/each}
	<div class="mt-auto grid gap-2 rounded-lg border bg-background p-2">
		<div class="flex items-center gap-2" role="status" title={overall.text}>
			<span class={['size-2 shrink-0 rounded-full', overall.dot]}></span>
			<span class="hidden min-w-0 flex-1 truncate text-xs sm:inline">{overall.text}</span>
		</div>
		{#if pairs.length}
			<button
				type="button"
				class="flex items-center justify-center gap-1.5 rounded-md border px-2 py-1 text-xs hover:bg-muted"
				aria-label={allPaused ? m.resume_all() : m.pause_all()}
				title={allPaused ? m.resume_all() : m.pause_all()}
				onclick={() => attempt(() => invoke('set_paused', { pair: null, paused: !allPaused }))}
			>
				{#if allPaused}
					<PlayIcon class="size-3.5 shrink-0" aria-hidden="true" />
				{:else}
					<PauseIcon class="size-3.5 shrink-0" aria-hidden="true" />
				{/if}
				<span class="hidden sm:inline">{allPaused ? m.resume_all() : m.pause_all()}</span>
			</button>
		{/if}
	</div>
</nav>
