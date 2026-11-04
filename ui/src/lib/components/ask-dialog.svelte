<script lang="ts">
	// The page's one dialog: `ask(...)` (app.svelte.ts) fills it and waits for
	// the choice; closing it answers null.
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { question } from '#lib/app.svelte.js';
	import { m } from '#lib/paraglide/messages.js';

	function reply(value: string | null) {
		question.open = false;
		question.answer(value);
	}
</script>

<Dialog.Root bind:open={question.open} onOpenChange={(now) => !now && question.answer(null)}>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>{question.title}</Dialog.Title>
			<Dialog.Description>{question.text}</Dialog.Description>
		</Dialog.Header>
		<Dialog.Footer>
			<Button variant="outline" onclick={() => reply(null)}>{m.cancel()}</Button>
			{#each question.choices as choice (choice.value)}
				<Button variant={choice.variant ?? 'default'} onclick={() => reply(choice.value)}>
					{choice.label}
				</Button>
			{/each}
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
