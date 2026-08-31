<script lang="ts">
	import { portal } from '$lib/portal';
	import { clampHorizontal } from '$lib/tooltip.svelte';

	interface Props {
		onAdd?: () => void;
		onAddFoil?: () => void;
		onAddWanted?: () => void;
		/** Opens a picker to add a specific printing (set/collector number) of this card instead. */
		onChoosePrinting?: () => void;
		/** Quantity that will be added, purely for the button labels (e.g. "Add 4"). Defaults to 1. */
		quantity?: number;
	}

	let { onAdd, onAddFoil, onAddWanted, onChoosePrinting, quantity = 1 }: Props = $props();

	let open = $state(false);
	let style = $state('');
	let btnEl: HTMLButtonElement | undefined = $state();

	// Guard against a blank/invalid/zero quantity making it into the label or the add call.
	const qty = $derived(Number.isFinite(quantity) && quantity > 0 ? Math.floor(quantity) : 1);

	function position() {
		if (!btnEl) return;
		const rect = btnEl.getBoundingClientRect();
		const menuH = 140;
		const margin = 6;
		const xStyle = clampHorizontal(rect, 160);
		const fitsBelow = rect.bottom + margin + menuH <= window.innerHeight;
		const yStyle = fitsBelow
			? `top: ${rect.bottom + margin}px;`
			: `bottom: ${window.innerHeight - rect.top + margin}px;`;
		style = `${yStyle} ${xStyle}`;
	}

	function toggle(e: MouseEvent) {
		e.stopPropagation();
		open = !open;
		if (open) position();
	}

	function pick(e: MouseEvent, fn?: () => void) {
		e.stopPropagation();
		open = false;
		fn?.();
	}
</script>

<button bind:this={btnEl} class="btn btn-sm btn-accent" title="Add to collection" onclick={toggle}>
	+ ▾
</button>

{#if open}
	<div
		use:portal
		class="add-dropdown-menu"
		role="menu"
		tabindex="-1"
		{style}
		onclick={(e) => e.stopPropagation()}
		onkeydown={(e) => { if (e.key === 'Escape') open = false; }}
	>
		{#if onAdd}
			<button class="add-dropdown-item" role="menuitem" onclick={(e) => pick(e, onAdd)}>Add {qty}</button>
		{/if}
		{#if onAddFoil}
			<button class="add-dropdown-item" role="menuitem" onclick={(e) => pick(e, onAddFoil)}>Add {qty} foil</button>
		{/if}
		{#if onAddWanted}
			<button class="add-dropdown-item" role="menuitem" onclick={(e) => pick(e, onAddWanted)}>Add {qty} wanted</button>
		{/if}
		{#if onChoosePrinting}
			<button class="add-dropdown-item" role="menuitem" onclick={(e) => pick(e, onChoosePrinting)}>Choose printing…</button>
		{/if}
	</div>
{/if}

<svelte:window onclick={() => { if (open) open = false; }} />
