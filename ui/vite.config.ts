import { paraglideVitePlugin } from '@inlang/paraglide-js';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [
		tailwindcss(),
		svelte(),
		paraglideVitePlugin({
			project: './project.inlang',
			outdir: './src/lib/paraglide',
			emitTsDeclarations: true,
			// English only for now: no locale to detect or remember.
			strategy: ['baseLocale']
		})
	],
	// tauri.conf.json's devUrl: taken, it fails rather than moves.
	server: { port: 5194, strictPort: true },
	// The Tauri webviews (WebKit on macOS and Linux, WebView2 on Windows).
	build: { target: ['safari16', 'chrome110'] },
	clearScreen: false
});
