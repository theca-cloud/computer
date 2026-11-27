// What the pages do to an account.
import { invoke } from '@tauri-apps/api/core';
import { ask, attempt, say, type Account } from '#lib/app.svelte.js';
import { m } from '#lib/paraglide/messages.js';

/** Removes an account, its files kept or deleted as the user chooses. */
export async function removeAccount(account: Account) {
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
