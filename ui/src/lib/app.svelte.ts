// The page's state and its link to the app (src/gui.rs): the commands go
// through `invoke`, the engine's changes come as `update` events.
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type { ButtonVariant } from '#lib/components/ui/button/index.js';

export type Account = {
	id: number;
	instance: string;
	email: string;
	device: string;
	default_folder: string;
	/** What libraries new to the account do. */
	new_libraries: NewLibraries;
	/** Bytes: new libraries up to it sync (`sync`); 0: no limit. */
	auto_sync_limit: number;
	signed_out: boolean;
};

export type NewLibraries = 'sync' | 'drive' | 'notify' | 'silent';

export type Pair = {
	id: number;
	account: number;
	library: string;
	library_name: string;
	path: string;
	paused: boolean;
	stopped: string | null;
	folder_stop: boolean;
	writable: boolean;
	activity: string;
	error: string | null;
	synced_at: string | null;
	/** A drive's: none for a sync pair. */
	drive: Drive | null;
};

/** A drive's numbers, as its OS can tell them (Linux, Windows: all; macOS: kept). */
export type Drive = {
	downloaded: number | null;
	kept: number;
	waiting: number | null;
	/** macOS added it switched off: the user switches it on. */
	switch_on: boolean;
};

export type Notice = {
	id: number;
	pair: number | null;
	at: string;
	level: 'error' | 'info';
	path: string | null;
	message: string;
	undo: boolean;
};

export type Status = {
	accounts: Account[];
	pairs: Pair[];
	notices: Notice[];
	/** By account ID (a JSON key): the libraries last offered. */
	offered: Record<string, Offer[]>;
};

/** A library the account sees and doesn't sync (an `offered` update). */
export type Offer = { library: string; name: string; size: number };

/** A library as `libraries` lists it (from the server). */
export type Library = {
	id: string;
	name: string;
	size: number | null;
	files: number | null;
	writable: boolean;
	pair: number | null;
};

type Update =
	| { kind: 'activity'; pair: number; activity: string }
	| { kind: 'pairs' | 'accounts' | 'notices' | 'lagged' }
	| { kind: 'offered'; account: number; libraries: Offer[] }
	| { kind: 'new_library'; account: number; name: string };

export const app = $state({
	status: null as Status | null,
	/** By account ID: the status's, then what each `offered` update says. */
	offered: {} as Record<string, Offer[]>,
	/** The last outcome or error, shown until dismissed. */
	message: null as { text: string; error: boolean } | null,
	/** Ticks, so "5 min ago" moves on. */
	now: Date.now()
});

export async function reload() {
	const status = await invoke<Status>('status');
	app.status = status;
	app.offered = status.offered;
}

export function say(text: string, error = false) {
	app.message = { text, error };
}

/** Runs a command; its error (a string, from Rust) is shown, and undefined returned. */
export async function attempt<T>(work: () => Promise<T>): Promise<T | undefined> {
	try {
		return await work();
	} catch (e) {
		say(String(e), true);
	}
}

/** Loads the status and follows the engine; returns the cleanup. */
export function follow(): () => void {
	const unlisten = listen<Update>('update', ({ payload }) => {
		if (payload.kind === 'activity') {
			const pair = app.status?.pairs.find((p) => p.id === payload.pair);
			if (pair) {
				pair.activity = payload.activity;
				// An activity ends the error the engine stopped on.
				pair.error = null;
			}
			// Its last synced time changed with it.
			if (payload.activity === 'up to date') void attempt(reload);
		} else if (payload.kind === 'offered') {
			app.offered[payload.account] = payload.libraries;
		} else if (payload.kind === 'new_library') {
			// The OS notification is Rust's; the list follows (`offered`).
		} else {
			void attempt(reload);
		}
	});
	void attempt(reload);
	const tick = setInterval(() => (app.now = Date.now()), 30_000);
	return () => {
		void unlisten.then((stop) => stop());
		clearInterval(tick);
	};
}

export type Choice = { label: string; value: string; variant?: ButtonVariant };

/** The question the dialog shows (ask-dialog.svelte). */
export const question = $state({
	open: false,
	title: '',
	text: '',
	choices: [] as Choice[],
	answer: (_: string | null) => {}
});

/** Asks; answers the chosen value, or null when cancelled. */
export function ask(title: string, text: string, choices: Choice[]): Promise<string | null> {
	return new Promise((answer) =>
		Object.assign(question, { open: true, title, text, choices, answer })
	);
}

/**
 * Starts the browser login (Rust opens the browser): the code to show, and
 * the account once approved (the `login` event).
 */
/** The account signed in, and whether it is new here (its first step shows). */
export type SignedIn = { account: number; fresh: boolean };

export async function signIn(
	instance: string,
	device: string,
	folder: string | null
): Promise<{ code: string; account: Promise<SignedIn> }> {
	let stop = () => {};
	const account = new Promise<SignedIn>((resolve, reject) => {
		const unlisten = listen<SignedIn | { error: string }>('login', ({ payload }) => {
			stop();
			if ('account' in payload) resolve(payload);
			else reject(payload.error);
		});
		stop = () => void unlisten.then((f) => f());
	});
	// A login failing before its flow exists rejects both: the command's error is shown.
	account.catch(() => {});
	try {
		const flow = await invoke<{ code: string }>('login', { instance, device, folder });
		return { code: flow.code, account };
	} catch (e) {
		stop();
		throw e;
	}
}
