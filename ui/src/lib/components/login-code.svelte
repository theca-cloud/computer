<script lang="ts">
	// While the browser login waits for its approval: the code the browser's
	// page must show too (it stops a phishing flow).
	import { Button } from '#lib/components/ui/button/index.js';
	import { m } from '#lib/paraglide/messages.js';

	let { code, oncancel }: { code: string; oncancel: () => void } = $props();
</script>

<div class="grid justify-items-start gap-3" role="status">
	<p class="text-sm">{m.login_waiting()}</p>
	<p class="font-mono text-3xl font-semibold tracking-widest">{code}</p>
	<p class="text-sm text-muted-foreground">{m.login_waiting_hint()}</p>
	<Button variant="outline" size="sm" onclick={oncancel}>{m.cancel()}</Button>
</div>
