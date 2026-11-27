// What a library's line says of its state.
import type { BadgeVariant } from '#lib/components/ui/badge/index.js';
import type { Pair } from '#lib/app.svelte.js';
import { m } from '#lib/paraglide/messages.js';

export type Shown = {
	text: string;
	variant: BadgeVariant;
	/** The dot's colour (a Tailwind class). */
	dot: string;
};

/** A pair's state as its line shows it: a word, and a colour. */
export function pairStatus(pair: Pair): Shown {
	if (pair.stopped)
		return {
			text: m.stopped({ reason: pair.stopped }),
			variant: 'destructive',
			dot: 'bg-destructive'
		};
	if (pair.paused) return { text: m.paused(), variant: 'secondary', dot: 'bg-muted-foreground' };
	if (pair.error)
		return {
			text: m.activity_error({ reason: pair.error }),
			variant: 'destructive',
			dot: 'bg-destructive'
		};
	const activities: Record<string, () => Shown> = {
		waiting: () => ({
			text: m.activity_waiting(),
			variant: 'secondary',
			dot: 'bg-muted-foreground'
		}),
		syncing: () => ({ text: m.activity_syncing(), variant: 'default', dot: 'bg-blue-500' }),
		'listing the library': () => ({
			text: m.activity_listing(),
			variant: 'default',
			dot: 'bg-blue-500'
		}),
		'up to date': () => ({ text: m.activity_up_to_date(), variant: 'outline', dot: 'bg-primary' }),
		offline: () => ({
			text: m.activity_offline(),
			variant: 'secondary',
			dot: 'bg-muted-foreground'
		}),
		'signed out': () => ({
			text: m.activity_signed_out(),
			variant: 'destructive',
			dot: 'bg-destructive'
		})
	};
	return (
		activities[pair.activity]?.() ?? {
			text: pair.activity,
			variant: 'secondary',
			dot: 'bg-muted-foreground'
		}
	);
}
