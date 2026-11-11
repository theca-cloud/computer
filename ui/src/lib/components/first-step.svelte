<script lang="ts">
	// A new account's first step, right after signing in: each library
	// synced (into its folder under the parent folder), a drive, or nothing;
	// what libraries new later do. Nothing is added before Add; Later adds
	// nothing (the libraries stay listed).
	import { onMount } from 'svelte';
	import { invoke } from '@tauri-apps/api/core';
	import { open } from '@tauri-apps/plugin-dialog';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import { ask, attempt, type Account, type NewLibraries } from '#lib/app.svelte.js';
	import { formatSize } from '#lib/format.js';
	import { m } from '#lib/paraglide/messages.js';

	let { account, ondone }: { account: Account; ondone: () => void } = $props();

	type Proposed = { id: string; name: string; size: number | null; path: string };
	type Choice = 'sync' | 'drive' | 'nothing';

	const MB = 1024 * 1024;
	// svelte-ignore state_referenced_locally
	let parent = $state(account.default_folder);
	let libraries = $state<Proposed[] | null>(null);
	let choices = $state<Record<string, Choice>>({});
	/** Folders replaced by hand: kept when the parent changes. */
	let replaced = $state<Record<string, string>>({});
	let newLibraries = $state<NewLibraries>('notify');
	let mb = $state(500);
	/** Why this computer can't hold a drive, if it can't. */
	let unavailable = $state<string | null>(null);
	let adding = $state(false);

	async function load() {
		// Add waits for the folders under this parent.
		libraries = null;
		libraries =
			(await attempt(() => invoke<Proposed[]>('first_step', { account: account.id, parent }))) ??
			[];
	}

	onMount(() => {
		void load();
		void attempt(() => invoke<string | null>('drive_support')).then(
			(why) => (unavailable = why ?? null)
		);
	});

	const pathOf = (l: Proposed) => replaced[l.id] ?? l.path;

	async function pickParent() {
		const picked = await attempt(() => open({ directory: true, defaultPath: parent }));
		if (typeof picked !== 'string') return;
		parent = picked;
		await load();
	}

	async function pickFolder(l: Proposed) {
		const picked = await attempt(() => open({ directory: true, defaultPath: pathOf(l) }));
		if (typeof picked === 'string') replaced[l.id] = picked;
	}

	async function add() {
		// An emptied size field: nothing saved (it would read as no limit).
		const size = typeof mb === 'number' && mb >= 0 ? Math.round(mb) : null;
		if (newLibraries === 'sync' && size === null) return;
		adding = true;
		const saved = await attempt(() =>
			invoke('set_account', {
				account: account.id,
				defaultFolder: parent,
				newLibraries,
				autoSyncLimit: (size ?? 500) * MB
			})
		);
		if (saved === undefined) {
			adding = false;
			return;
		}
		for (const l of libraries ?? []) {
			const choice = choices[l.id] ?? 'nothing';
			if (choice === 'nothing') continue;
			const once = (merge: boolean) =>
				invoke<{ pair: number } | { holds: number }>('add', {
					account: account.id,
					library: l.id,
					path: choice === 'sync' ? pathOf(l) : null,
					merge,
					drive: choice === 'drive'
				});
			const added = await attempt(() => once(false));
			if (added && 'holds' in added) {
				const merge = await ask(
					m.merge_title({ files: added.holds }),
					m.merge_text({ library: l.name }),
					[{ label: m.merge(), value: 'merge' }]
				);
				if (merge) await attempt(() => once(true));
			}
		}
		adding = false;
		ondone();
	}

	const options: { value: Choice; label: () => string }[] = [
		{ value: 'sync', label: m.first_sync },
		{ value: 'drive', label: m.drive },
		{ value: 'nothing', label: m.first_nothing }
	];
</script>

<Card.Root id="first-step">
	<Card.Header>
		<Card.Title>{m.first_title()}</Card.Title>
		<Card.Description>{m.first_text({ email: account.email })}</Card.Description>
	</Card.Header>
	<Card.Content class="grid gap-4">
		<div class="grid gap-1.5">
			<span class="text-sm">{m.first_parent()}</span>
			<div class="flex items-center gap-2">
				<p class="min-w-0 flex-1 truncate text-muted-foreground" title={parent}>{parent}</p>
				<Button size="sm" variant="outline" onclick={pickParent}>{m.change()}</Button>
			</div>
		</div>

		{#if libraries === null}
			<p class="text-muted-foreground" role="status">{m.loading()}</p>
		{:else if libraries.length === 0}
			<p class="text-muted-foreground">{m.libraries_none()}</p>
		{:else}
			<ul class="grid gap-3" aria-label={m.libraries()}>
				{#each libraries as l (l.id)}
					{@const choice = choices[l.id] ?? 'nothing'}
					<li class="grid gap-1">
						<div class="flex flex-wrap items-center justify-between gap-2">
							<span class="min-w-0 truncate font-medium">
								{l.name}
								{#if l.size !== null}
									<span class="font-normal text-muted-foreground"> · {formatSize(l.size)}</span>
								{/if}
							</span>
							<div class="flex gap-3" role="radiogroup" aria-label={l.name}>
								{#each options as o (o.value)}
									{@const off = o.value === 'drive' && unavailable !== null}
									<label
										class="flex items-center gap-1 text-xs"
										title={off ? (unavailable ?? '') : ''}
									>
										<input
											type="radio"
											id="choice-{l.id}-{o.value}"
											name="choice-{l.id}"
											value={o.value}
											disabled={off}
											checked={choice === o.value}
											onchange={() => (choices[l.id] = o.value)}
										/>
										{o.label()}
									</label>
								{/each}
							</div>
						</div>
						{#if choice === 'sync'}
							<div class="flex items-center gap-2">
								<p class="min-w-0 flex-1 truncate text-xs text-muted-foreground" title={pathOf(l)}>
									{pathOf(l)}
								</p>
								<Button size="sm" variant="ghost" onclick={() => pickFolder(l)}>
									{m.change()}
								</Button>
							</div>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}

		<div class="grid gap-1.5">
			<Label for="new-libraries">{m.first_new()}</Label>
			<div class="flex flex-wrap items-center gap-2">
				<select
					id="new-libraries"
					class="h-8 rounded-md border bg-background px-2 text-sm"
					bind:value={newLibraries}
				>
					<option value="notify">{m.new_notify()}</option>
					<option value="silent">{m.new_silent()}</option>
					<option value="sync">{m.new_sync()}</option>
					<option value="drive" disabled={unavailable !== null}>{m.new_drive()}</option>
				</select>
				{#if newLibraries === 'sync'}
					<Label for="new-limit" class="text-xs">{m.new_limit()}</Label>
					<Input id="new-limit" type="number" min="0" class="w-24" bind:value={mb} />
					<span class="text-xs text-muted-foreground">{m.auto_sync_unit()}</span>
				{/if}
			</div>
			{#if newLibraries === 'sync'}
				<p class="text-xs text-muted-foreground">{m.new_limit_hint()}</p>
			{/if}
		</div>
	</Card.Content>
	<Card.Footer class="gap-2">
		<Button disabled={adding || libraries === null} onclick={add}>{m.first_add()}</Button>
		<Button variant="ghost" disabled={adding} onclick={ondone}>{m.first_later()}</Button>
	</Card.Footer>
</Card.Root>
