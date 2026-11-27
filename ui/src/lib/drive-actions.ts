// What a drive's menu and its details both do.
import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import { attempt, type Pair } from '#lib/app.svelte.js';

/** Keeps the files picked in a drive (`theca keep`): how many were picked. */
export async function keepFiles(pair: Pair): Promise<number> {
	const picked = await attempt(() =>
		open({ defaultPath: pair.path, multiple: true, directory: false })
	);
	const paths = Array.isArray(picked) ? picked : typeof picked === 'string' ? [picked] : [];
	for (const path of paths) await attempt(() => invoke('keep', { path }));
	return paths.length;
}
