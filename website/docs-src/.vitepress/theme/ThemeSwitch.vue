<script setup lang="ts">
// Three-state theme switch (system, light, dark) matching the landing page.
// VitePress's own switch only toggles light and dark. Its dark mode reads the
// same storage key, and a storage event makes it pick up the new value.
import { computed, onMounted, ref } from "vue";

type Mode = "auto" | "light" | "dark";

const KEY = "vitepress-theme-appearance";
const MODES: Mode[] = ["auto", "light", "dark"];
const NAMES: Record<Mode, string> = {
  auto: "system",
  light: "light",
  dark: "dark",
};

const mode = ref<Mode>("auto");
const label = computed(() => `Theme: ${NAMES[mode.value]}`);

onMounted(() => {
  const stored = localStorage.getItem(KEY);
  if (stored === "light" || stored === "dark") {
    mode.value = stored;
  }
});

function cycle() {
  const next = MODES[(MODES.indexOf(mode.value) + 1) % MODES.length];
  mode.value = next;
  localStorage.setItem(KEY, next);
  window.dispatchEvent(
    new StorageEvent("storage", {
      key: KEY,
      newValue: next,
      storageArea: localStorage,
    }),
  );
}
</script>

<template>
  <button
    class="theme"
    type="button"
    :aria-label="label"
    :title="label"
    @click="cycle"
  >
    <svg viewBox="0 0 16 16" aria-hidden="true">
      <circle
        cx="8"
        cy="8"
        r="6.25"
        fill="none"
        stroke="currentColor"
        stroke-width="1.5"
      />
      <path v-if="mode === 'auto'" d="M8 2a6 6 0 0 0 0 12z" fill="currentColor" />
      <circle v-if="mode === 'dark'" cx="8" cy="8" r="6" fill="currentColor" />
    </svg>
  </button>
</template>

<style scoped>
.theme {
  display: flex;
  margin-left: 16px;
  padding: 2px;
  border: 0;
  background: none;
  color: var(--vp-c-text-2);
  cursor: pointer;
}

.theme:hover {
  color: var(--vp-c-text-1);
}

.theme:focus-visible {
  outline: 2px solid var(--vp-c-brand-1);
  outline-offset: 3px;
}

.theme svg {
  width: 14px;
  height: 14px;
}
</style>
