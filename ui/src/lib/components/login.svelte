<script lang="ts">
	// Signs a new account in: the instance, this device's name and the
	// account's default folder, then the browser login (its code shown).
	import { invoke } from '@tauri-apps/api/core';
	import { open } from '@tauri-apps/plugin-dialog';
	import LoginCode from '#lib/components/login-code.svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import { attempt, say, signIn } from '#lib/app.svelte.js';
	import { m } from '#lib/paraglide/messages.js';

	let {
		onaccount,
		oncancel
	}: {
		/** Signed in: the account. */
		onaccount: (account: number) => void;
		/** Absent on the first launch: there is nothing to go back to. */
		oncancel?: () => void;
	} = $props();

	let instance = $state('');
	let device = $state('');
	let folder = $state('');
	let code = $state('');
	let busy = $state(false);
	/** The folder chosen by hand: the proposal no longer replaces it. */
	let chosen = false;
	/** Bumped by Cancel: a login approved after it is ignored here. */
	let attempts = 0;

	// The proposal depends on the instance (a second account gets
	// ~/theca-<host>); the device name is the hostname.
	$effect(() => {
		void attempt(() =>
			invoke<{ folder: string; device: string }>('proposed_folder', { instance })
		).then((proposal) => {
			if (!proposal) return;
			if (!chosen) folder = proposal.folder;
			device ||= proposal.device;
		});
	});

	async function pick() {
		const picked = await attempt(() => open({ directory: true, defaultPath: folder }));
		if (typeof picked !== 'string') return;
		folder = picked;
		chosen = true;
	}

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		const started = await attempt(() => signIn(instance.trim(), device.trim(), folder || null));
		busy = false;
		if (!started) return;
		code = started.code;
		const mine = ++attempts;
		started.account.then(
			(account) => mine === attempts && onaccount(account),
			(error) => {
				if (mine !== attempts) return;
				code = '';
				say(String(error), true);
			}
		);
	}

	function cancel() {
		attempts++;
		code = '';
	}
</script>

{#if code}
	<LoginCode {code} oncancel={cancel} />
{:else}
	<form class="grid gap-4" onsubmit={submit}>
		<div class="grid gap-1.5">
			<Label for="instance">{m.login_instance()}</Label>
			<Input
				id="instance"
				type="url"
				required
				placeholder="https://cloud.example.com"
				autocomplete="url"
				bind:value={instance}
			/>
		</div>
		<div class="grid gap-1.5">
			<Label for="device">{m.login_device()}</Label>
			<Input id="device" required bind:value={device} />
			<p class="text-xs text-muted-foreground">{m.login_device_hint()}</p>
		</div>
		<div class="grid gap-1.5">
			<Label for="folder">{m.default_folder()}</Label>
			<div class="flex gap-2">
				<Input id="folder" required bind:value={folder} oninput={() => (chosen = true)} />
				<Button variant="outline" onclick={pick}>{m.change()}</Button>
			</div>
			<p class="text-xs text-muted-foreground">{m.default_folder_hint()}</p>
		</div>
		<div class="flex gap-2">
			<Button type="submit" disabled={busy}>{m.sign_in()}</Button>
			{#if oncancel}
				<Button variant="ghost" onclick={oncancel}>{m.cancel()}</Button>
			{/if}
		</div>
	</form>
{/if}
