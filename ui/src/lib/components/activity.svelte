<script lang="ts">
	// The Activity page: what Theca tells, errors first, newest first.
	import { invoke } from '@tauri-apps/api/core';
	import { Button } from '#lib/components/ui/button/index.js';
	import { app, attempt, say } from '#lib/app.svelte.js';
	import { formatDate } from '#lib/format.js';
	import { m } from '#lib/paraglide/messages.js';

	const status = $derived(app.status);
	const pairs = $derived(status?.pairs ?? []);
	// Errors first, newest first.
	const notices = $derived(
		[...(status?.notices ?? [])].sort(
			(a, b) =>
				Number(b.level === 'error') - Number(a.level === 'error') || b.at.localeCompare(a.at)
		)
	);
	const libraryOf = (pair: number | null) => pairs.find((p) => p.id === pair)?.library_name;

	async function undo(notice: number) {
		const restored = await attempt(() => invoke<number>('undo', { notice }));
		if (restored !== undefined) say(m.restored({ count: restored }));
	}
</script>

<div class="grid gap-4">
	<h1 class="text-lg font-semibold">{m.nav_activity()}</h1>
	{#if notices.length}
		<ul class="grid gap-2" aria-label={m.notices()}>
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
	{:else}
		<p class="text-muted-foreground">{m.activity_none()}</p>
	{/if}
</div>
