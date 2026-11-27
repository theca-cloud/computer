<script lang="ts">
	// The Settings page: this computer's (the drive limit, the logs, the
	// updates, start at login) and Quit.
	import { onMount } from 'svelte';
	import { invoke } from '@tauri-apps/api/core';
	import DriveSettings from '#lib/components/drive-settings.svelte';
	import LogsSettings from '#lib/components/logs-settings.svelte';
	import UpdatesSettings from '#lib/components/updates-settings.svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import { Separator } from '#lib/components/ui/separator/index.js';
	import { Switch } from '#lib/components/ui/switch/index.js';
	import { attempt } from '#lib/app.svelte.js';
	import { m } from '#lib/paraglide/messages.js';

	let autostart = $state(false);

	onMount(() => {
		void attempt(() => invoke<boolean>('autostart_enabled')).then(
			(on) => (autostart = on ?? false)
		);
	});

	async function setAutostart(on: boolean) {
		autostart = (await attempt(() => invoke<boolean>('set_autostart', { on }))) ?? autostart;
	}
</script>

<div class="grid gap-4">
	<h1 class="text-lg font-semibold">{m.nav_settings()}</h1>
	<DriveSettings />
	<Separator />
	<LogsSettings />
	<Separator />
	<UpdatesSettings />
	<Separator />
	<div class="flex items-center gap-2">
		<Switch id="autostart" checked={autostart} onCheckedChange={setAutostart} />
		<Label for="autostart">{m.autostart()}</Label>
	</div>
	<div>
		<Button variant="destructive" onclick={() => attempt(() => invoke('quit'))}>
			{m.quit()}
		</Button>
	</div>
</div>
