<script lang="ts">
	import type { MtgCard } from '$lib/types';
	import { getMtgPrintings } from '$lib/api';
	import { cardImageUrl } from '$lib/types';
	import { syncCachedImageUrl } from '$lib/imageCache';

	interface Props {
		/** Exact card name to list every printing of. */
		cardName: string;
		onSelect: (card: MtgCard) => void;
		onclose: () => void;
	}

	let { cardName, onSelect, onclose }: Props = $props();

	let printings = $state<MtgCard[]>([]);
	let loading = $state(true);
	let error = $state('');

	$effect(() => {
		loading = true;
		error = '';
		getMtgPrintings(cardName)
			.then(results => {
				// De-dupe: the SQL and Scryfall backends can both surface the same
				// printing under different provider IDs, which would otherwise show
				// up as look-alike duplicate rows.
				const seen = new Set<string>();
				printings = results.filter(c => {
					const key = `${(c.setCode || '').toLowerCase()}#${c.collectorNumber || ''}`;
					if (seen.has(key)) return false;
					seen.add(key);
					return true;
				});
			})
			.catch(() => { error = 'Failed to load printings.'; })
			.finally(() => { loading = false; });
	});

	function thumb(card: MtgCard): string {
		const raw = cardImageUrl(card);
		if (!raw) return '';
		return syncCachedImageUrl(raw) || raw;
	}
</script>

<div class="modal-overlay" onclick={(e) => e.target === e.currentTarget && onclose()}>
	<div class="modal" style="max-width: 640px;">
		<div class="modal-header">
			<h3>Choose printing — {cardName}</h3>
			<button class="btn btn-ghost btn-sm" onclick={onclose}>✕</button>
		</div>
		<div class="modal-body">
			{#if loading}
				<p style="color: var(--text2); font-size: 0.9rem;">Loading printings…</p>
			{:else if error}
				<p style="color: var(--danger, #e5484d); font-size: 0.9rem;">{error}</p>
			{:else if printings.length === 0}
				<p style="color: var(--text2); font-size: 0.9rem;">No printings found for this card.</p>
			{:else}
				<p style="color: var(--text2); font-size: 0.82rem; margin-bottom: 12px;">
					{printings.length} printing{printings.length === 1 ? '' : 's'} found. Pick the one you have.
				</p>
				<div style="display:flex; flex-direction:column; gap:6px; max-height: 60vh; overflow-y:auto;">
					{#each printings as printing (printing.id)}
						<button
							class="printing-row"
							onclick={() => onSelect(printing)}
						>
							{#if thumb(printing)}
								<img src={thumb(printing)} alt="" class="printing-thumb" loading="lazy" />
							{:else}
								<div class="printing-thumb printing-thumb-placeholder"></div>
							{/if}
							<div style="flex:1; min-width:0; text-align:left;">
								<div style="font-size:0.9rem; font-weight:600; color:var(--text);">
									{printing.setName || printing.setCode?.toUpperCase() || 'Unknown set'}
								</div>
								<div style="font-size:0.78rem; color:var(--text2);">
									{printing.setCode?.toUpperCase()} · #{printing.collectorNumber ?? '?'} · {printing.rarity}
								</div>
							</div>
							<span class="btn btn-sm btn-accent" style="pointer-events:none;">Select</span>
						</button>
					{/each}
				</div>
			{/if}
		</div>
	</div>
</div>

<style>
	.printing-row {
		display: flex;
		align-items: center;
		gap: 10px;
		width: 100%;
		padding: 8px 10px;
		background: var(--surface);
		border: 1px solid var(--border);
		border-radius: var(--radius);
		cursor: pointer;
		text-align: left;
		font: inherit;
		color: inherit;
	}
	.printing-row:hover {
		border-color: var(--accent);
		background: var(--surface-hover, var(--surface));
	}
	.printing-thumb {
		width: 40px;
		height: 56px;
		object-fit: cover;
		border-radius: 4px;
		flex-shrink: 0;
		background: var(--bg2);
	}
	.printing-thumb-placeholder {
		border: 1px dashed var(--border2);
	}
</style>
