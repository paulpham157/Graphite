<script lang="ts">
	import { getContext } from "svelte";

	import type { Editor } from "@graphite/editor";
	import type { FullscreenState } from "@graphite/state-providers/fullscreen";

	import LayoutRow from "@graphite/components/layout/LayoutRow.svelte";
	import IconLabel from "@graphite/components/widgets/labels/IconLabel.svelte";
	import TextLabel from "@graphite/components/widgets/labels/TextLabel.svelte";

	const editor = getContext<Editor>("editor");
	const fullscreen = getContext<FullscreenState>("fullscreen");

	$: requestFullscreenHotkeys = fullscreen.keyboardLockApiSupported && !$fullscreen.keyboardLocked;

	async function handleClick(e: MouseEvent) {
		// TODO: Remove this debugging option to switch from web to desktop window buttons
		if (e.ctrlKey && e.shiftKey && e.altKey) {
			editor.handle.appWindowMinimize();
			editor.handle.appWindowMinimize();
			return;
		}

		if ($fullscreen.windowFullscreen) fullscreen.exitFullscreen();
		else fullscreen.enterFullscreen();
	}
</script>

<LayoutRow class="window-buttons-web" on:click={handleClick} tooltip={$fullscreen.windowFullscreen ? "Exit Fullscreen (F11)" : "Enter Fullscreen (F11)"}>
	{#if requestFullscreenHotkeys}
		<TextLabel italic={true}>Go fullscreen to access all hotkeys</TextLabel>
	{/if}
	<IconLabel icon={$fullscreen.windowFullscreen ? "FullscreenExit" : "FullscreenEnter"} />
</LayoutRow>

<style lang="scss" global>
	.window-buttons-web {
		flex: 0 0 auto;
		align-items: center;
		padding: 0 8px;

		svg {
			fill: var(--color-e-nearwhite);
		}

		.text-label {
			margin-right: 8px;
		}

		&:hover {
			background: var(--color-6-lowergray);
			color: var(--color-f-white);

			svg {
				fill: var(--color-f-white);
			}
		}
	}
</style>
