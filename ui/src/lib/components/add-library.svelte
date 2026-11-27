<script lang="ts">
	// The dialog that adds a library: the account's libraries that aren't on
	// this computer, each synced to a folder or kept as a drive.
	import { invoke } from '@tauri-apps/api/core';
	import { open as pick } from '@tauri-apps/plugin-dialog';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import { Switch } from '#lib/components/ui/switch/index.js';
	import { ask, attempt, type Account, type Library } from '#lib/app.svelte.js';
	import { formatSize } from '#lib/format.js';
	import { m } from '#lib/paraglide/messages.js';

	let {
		account,
		synced,
		open = $bindable(false),
		unavailable
	}: {
		account: Account;
		/** The libraries already on this computer, by ID. */
		synced: Set<string>;
		open: boolean;
		/** Why this computer can't hold a drive (the Drive tick greyed), if it can't. */
		unavailable: string | null;
	} = $props();

	/** The server's list, asked for each time the dialog opens. */
	let libraries = $state<Library[] | null>(null);
	const unsynced = $derived(libraries?.filter((l) => !synced.has(l.id)) ?? []);
	let loading = $state(false);
	/** The libraries ticked as drives. */
	let asDrive = $state<Record<string, boolean>>({});

	$effect(() => {
		if (open) void list();
	});

	async function list() {
		loading = true;
		libraries =
			(await attempt(() => invoke<Library[]>('libraries', { account: account.id }))) ?? null;
		loading = false;
	}

	/** Syncs a library to its default folder, or (`choose`) to one picked. */
	async function sync(library: string, name: string, choose = false) {
		let path: string | null = null;
		if (choose) {
			const picked = await attempt(() =>
				pick({ directory: true, defaultPath: account.default_folder })
			);
			if (typeof picked !== 'string') return;
			path = picked;
		}
		const add = (merge: boolean) =>
			invoke<{ pair: number } | { holds: number }>('add', {
				account: account.id,
				library,
				path,
				merge,
				drive: asDrive[library] ?? false
			});
		const added = await attempt(() => add(false));
		if (!added) return;
		// Done, or a question to answer: either way the list is behind us.
		open = false;
		if ('holds' in added) {
			const merge = await ask(
				m.merge_title({ files: added.holds }),
				m.merge_text({ library: name }),
				[{ label: m.merge(), value: 'merge' }]
			);
			if (merge) await attempt(() => add(true));
		}
	}
</script>

<Dialog.Root bind:open>
	<Dialog.Content class="max-h-[85vh] overflow-y-auto sm:max-w-lg">
		<Dialog.Header>
			<Dialog.Title>{m.add_library_title()}</Dialog.Title>
			<Dialog.Description>{m.add_library_text({ email: account.email })}</Dialog.Description>
		</Dialog.Header>
		{#if loading && !libraries}
			<p class="text-muted-foreground" role="status">{m.loading()}</p>
		{:else if unsynced.length}
			<ul class="grid gap-3" aria-label={m.libraries()}>
				{#each unsynced as library (library.id)}
					<li class="grid gap-2 rounded-lg border p-3">
						<span class="min-w-0 truncate font-medium">
							{library.name}
							{#if library.size !== null}
								<span class="font-normal text-muted-foreground">
									· {formatSize(library.size)}
								</span>
							{/if}
						</span>
						<div class="flex flex-wrap items-center justify-between gap-2">
							<div class="flex items-center gap-1.5" title={unavailable ?? m.drive_hint()}>
								<Switch
									id="drive-{library.id}"
									aria-label={m.drive_library({ library: library.name })}
									disabled={unavailable !== null}
									bind:checked={
										() => asDrive[library.id] ?? false, (on) => (asDrive[library.id] = on)
									}
								/>
								<Label for="drive-{library.id}" class="text-xs">{m.drive()}</Label>
							</div>
							<div class="flex items-center gap-1">
								<!-- A drive takes no folder: it is in the drives folder. -->
								{#if !asDrive[library.id]}
									<Button
										size="sm"
										variant="ghost"
										aria-label={m.sync_library_to({ library: library.name })}
										onclick={() => sync(library.id, library.name, true)}
									>
										{m.sync_to()}
									</Button>
								{/if}
								<Button
									size="sm"
									aria-label={m.sync_library({ library: library.name })}
									onclick={() => sync(library.id, library.name)}
								>
									{m.sync()}
								</Button>
							</div>
						</div>
					</li>
				{/each}
			</ul>
		{:else}
			<p class="text-muted-foreground">{m.libraries_none()}</p>
		{/if}
	</Dialog.Content>
</Dialog.Root>
