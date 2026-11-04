<script lang="ts">
	// An account: who and where, signed out or not; its synced folders
	// (pairs); the libraries it could sync.
	import { onMount } from 'svelte';
	import { invoke } from '@tauri-apps/api/core';
	import { open } from '@tauri-apps/plugin-dialog';
	import LoginCode from '#lib/components/login-code.svelte';
	import { Badge, type BadgeVariant } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import {
		app,
		ask,
		attempt,
		say,
		signIn,
		type Account,
		type Library,
		type Offer,
		type Pair
	} from '#lib/app.svelte.js';
	import { ago, formatSize } from '#lib/format.js';
	import { m } from '#lib/paraglide/messages.js';

	let {
		account,
		pairs,
		offered,
		browse = false
	}: {
		account: Account;
		pairs: Pair[];
		offered: Offer[];
		/** Opens the list of its libraries (just signed in). */
		browse?: boolean;
	} = $props();

	const synced = $derived(new Set(pairs.map((p) => p.library)));
	const offers = $derived(offered.filter((o) => !synced.has(o.library)));
	/** The server's list, once asked for ("Add a library…"). */
	let libraries = $state<Library[] | null>(null);
	const unsynced = $derived(libraries?.filter((l) => !synced.has(l.id)) ?? []);
	let loading = $state(false);
	let code = $state('');
	let attempts = 0;

	onMount(() => {
		if (browse) void list();
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
				open({ directory: true, defaultPath: account.default_folder })
			);
			if (typeof picked !== 'string') return;
			path = picked;
		}
		const add = (merge: boolean) =>
			invoke<{ pair: number } | { holds: number }>('add', {
				account: account.id,
				library,
				path,
				merge
			});
		const added = await attempt(() => add(false));
		if (added && 'holds' in added) {
			const merge = await ask(
				m.merge_title({ files: added.holds }),
				m.merge_text({ library: name }),
				[{ label: m.merge(), value: 'merge' }]
			);
			if (merge) await attempt(() => add(true));
		}
	}

	function status(pair: Pair): { text: string; variant: BadgeVariant } {
		if (pair.stopped) return { text: m.stopped({ reason: pair.stopped }), variant: 'destructive' };
		if (pair.paused) return { text: m.paused(), variant: 'secondary' };
		const activities: Record<string, () => { text: string; variant: BadgeVariant }> = {
			waiting: () => ({ text: m.activity_waiting(), variant: 'secondary' }),
			syncing: () => ({ text: m.activity_syncing(), variant: 'default' }),
			'listing the library': () => ({ text: m.activity_listing(), variant: 'default' }),
			'up to date': () => ({ text: m.activity_up_to_date(), variant: 'outline' }),
			offline: () => ({ text: m.activity_offline(), variant: 'secondary' }),
			'signed out': () => ({ text: m.activity_signed_out(), variant: 'destructive' })
		};
		return activities[pair.activity]?.() ?? { text: pair.activity, variant: 'secondary' };
	}

	const setPaused = (pair: Pair, paused: boolean) =>
		attempt(() => invoke('set_paused', { pair: pair.id, paused }));

	/** Stopped over its folder (missing, trashed, unreadable): not the library's fault. */
	const folderStop = (reason: string) => !['library deleted', 'access removed'].includes(reason);

	/** Points a stopped pair at its folder, moved or another one (confirmed). */
	async function locate(pair: Pair) {
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

	async function remove(pair: Pair) {
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

	async function removeAccount() {
		const choice = await ask(
			m.remove_account_title({ email: account.email }),
			m.remove_account_text(),
			[
				{ label: m.delete_files(), value: 'delete', variant: 'destructive' },
				{ label: m.keep_files(), value: 'keep' }
			]
		);
		if (!choice) return;
		const kept = await attempt(() =>
			invoke<number>('remove_account', { account: account.id, deleteFiles: choice === 'delete' })
		);
		if (kept) say(m.kept({ count: kept }));
	}

	async function signInAgain() {
		const started = await attempt(() => signIn(account.instance, account.device, null));
		if (!started) return;
		code = started.code;
		const mine = ++attempts;
		started.account.then(
			() => mine === attempts && (code = ''),
			(error) => {
				if (mine !== attempts) return;
				code = '';
				say(String(error), true);
			}
		);
	}
</script>

{#snippet offer(id: string, name: string, size: number | null)}
	<li class="flex items-center justify-between gap-2">
		<span class="min-w-0 truncate">
			{name}
			{#if size !== null}<span class="text-muted-foreground"> · {formatSize(size)}</span>{/if}
		</span>
		<div class="flex shrink-0 gap-1">
			<Button
				size="sm"
				variant="ghost"
				aria-label={m.sync_library_to({ library: name })}
				onclick={() => sync(id, name, true)}
			>
				{m.sync_to()}
			</Button>
			<Button
				size="sm"
				variant="outline"
				aria-label={m.sync_library({ library: name })}
				onclick={() => sync(id, name)}
			>
				{m.sync()}
			</Button>
		</div>
	</li>
{/snippet}

<Card.Root>
	<Card.Header>
		<Card.Title class="truncate">{account.email}</Card.Title>
		<Card.Description class="truncate">{account.instance}</Card.Description>
	</Card.Header>
	<Card.Content class="grid gap-4">
		{#if account.signed_out}
			<div class="grid gap-2 rounded-lg bg-destructive/10 p-3" role="alert">
				<p>{m.signed_out({ instance: account.instance })}</p>
				{#if code}
					<LoginCode {code} oncancel={() => (attempts++, (code = ''))} />
				{:else}
					<div class="flex flex-wrap gap-2">
						<Button size="sm" onclick={signInAgain}>{m.sign_in_again()}</Button>
						<Button size="sm" variant="destructive" onclick={removeAccount}
							>{m.remove_account()}</Button
						>
					</div>
				{/if}
			</div>
		{/if}

		{#if pairs.length}
			<ul class="grid gap-3" aria-label={m.synced_folders()}>
				{#each pairs as pair (pair.id)}
					{@const badge = status(pair)}
					<li class="grid gap-1">
						<div class="flex flex-wrap items-center gap-2">
							<span class="font-medium">{pair.library_name}</span>
							<Badge variant={badge.variant} class="max-w-full truncate">{badge.text}</Badge>
							{#if !pair.writable}<Badge variant="outline">{m.read_only()}</Badge>{/if}
						</div>
						<p class="truncate text-xs text-muted-foreground" title={pair.path}>{pair.path}</p>
						<div class="flex flex-wrap items-center justify-between gap-2">
							<p class="text-xs text-muted-foreground">
								{pair.synced_at
									? m.last_synced({ when: ago(pair.synced_at, app.now) })
									: m.never_synced()}
							</p>
							<div class="flex gap-1">
								{#if !pair.stopped}
									<Button
										size="sm"
										variant="ghost"
										aria-label="{pair.paused ? m.resume() : m.pause()} {pair.library_name}"
										onclick={() => setPaused(pair, !pair.paused)}
									>
										{pair.paused ? m.resume() : m.pause()}
									</Button>
								{:else if folderStop(pair.stopped)}
									<Button
										size="sm"
										variant="ghost"
										aria-label="{m.try_again()} {pair.library_name}"
										onclick={() => setPaused(pair, false)}
									>
										{m.try_again()}
									</Button>
									<Button
										size="sm"
										variant="outline"
										aria-label="{m.locate()} {pair.library_name}"
										onclick={() => locate(pair)}
									>
										{m.locate()}
									</Button>
								{/if}
								<Button
									size="sm"
									variant="ghost"
									aria-label="{m.remove()} {pair.library_name}"
									onclick={() => remove(pair)}
								>
									{m.remove()}
								</Button>
							</div>
						</div>
					</li>
				{/each}
			</ul>
		{:else}
			<p class="text-muted-foreground">{m.no_pairs()}</p>
		{/if}

		{#if offers.length}
			<div class="grid gap-2">
				<h3 class="font-medium">{m.offered()}</h3>
				<p class="text-xs text-muted-foreground">{m.offered_hint()}</p>
				<ul class="grid gap-2">
					{#each offers as o (o.library)}
						{@render offer(o.library, o.name, o.size)}
					{/each}
				</ul>
			</div>
		{/if}

		{#if libraries}
			<div class="grid gap-2">
				<h3 class="font-medium">{m.libraries()}</h3>
				{#if unsynced.length}
					<ul class="grid gap-2">
						{#each unsynced as library (library.id)}
							{@render offer(library.id, library.name, library.size)}
						{/each}
					</ul>
				{:else}
					<p class="text-muted-foreground">{m.libraries_none()}</p>
				{/if}
			</div>
		{/if}
	</Card.Content>
	{#if !account.signed_out}
		<Card.Footer class="gap-2">
			<Button size="sm" variant="outline" disabled={loading} onclick={list}>
				{libraries ? m.libraries_refresh() : m.libraries_add()}
			</Button>
			<Button size="sm" variant="ghost" class="ml-auto" onclick={removeAccount}>
				{m.remove_account()}
			</Button>
		</Card.Footer>
	{/if}
</Card.Root>
