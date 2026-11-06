// Sizes and times as the page shows them.

const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];

/** `1.2 MiB`: powers of 1024, as the CLI and the web app show them; one decimal below 10. */
export function formatSize(bytes: number): string {
	let value = bytes;
	let unit = 0;
	while (value >= 1024 && unit < units.length - 1) {
		value /= 1024;
		unit++;
	}
	const shown = unit === 0 || value >= 10 ? Math.round(value) : Math.round(value * 10) / 10;
	return `${shown} ${units[unit]}`;
}

const dates = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' });

export function formatDate(value: string): string {
	return dates.format(new Date(value));
}

const relative = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto', style: 'short' });
const steps = [
	['day', 86_400],
	['hour', 3_600],
	['minute', 60]
] as const;

/** "5 min ago", "yesterday"; under a minute, "now". */
export function ago(value: string, now: number): string {
	const seconds = (new Date(value).getTime() - now) / 1000;
	for (const [unit, size] of steps) {
		if (Math.abs(seconds) >= size) return relative.format(Math.round(seconds / size), unit);
	}
	return relative.format(0, 'second');
}
