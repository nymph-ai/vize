<script setup>
import { computed, ref } from 'vue'

const props = defineProps({
  rows: Array,
  cursor: String,
  command: String,
  onChoose: Function,
  onOutside: Function,
})
const hovered = ref(null)
const rootClass = computed(() => hovered.value === null ? 'slice-shell' : 'slice-shell active')
</script>

<template>
  <section id="syrinx-table-root" :class="rootClass" data-v-syrinx-v22>
    <header class="slice-header" data-v-syrinx-v22>
      <strong>syrinx-live v3b</strong>
      <span class="cursor">{{ props.cursor }}</span>
      <span class="command">{{ props.command }}</span>
    </header>
    <div id="slice-scroll" class="slice-scroll" tabindex="0" data-v-syrinx-v22>
      <table id="slice-table" data-v-syrinx-v22>
        <thead><tr><th>row</th><th>value</th><th>state</th></tr></thead>
        <tbody>
          <tr
            v-for="row in props.rows"
            :key="row.id"
            :class="row.pending ? 'slice-row pending' : 'slice-row'"
            :data-row-key="row.id"
            :data-row-value-state="row.value"
            :data-row-status-state="row.status"
            data-v-syrinx-v22
            @mouseenter="hovered = row.id"
          >
            <td><span class="row-id">{{ row.id }}</span></td>
            <td>
              <button
                type="button"
                :class="{ 'slice-cell': true, sparkle: hovered === row.id }"
                tabindex="0"
                @click.stop="props.onChoose(row.id)"
              >
                <span>{{ row.value }}</span>
                <span v-if="row.pending" class="pending">local</span>
              </button>
            </td>
            <td><span class="row-status">{{ row.status }}</span></td>
          </tr>
        </tbody>
      </table>
    </div>
    <div id="slice-outside" class="slice-outside" data-v-syrinx-v22 @click="props.onOutside()"></div>
  </section>
</template>

<style scoped>
[data-v-syrinx-v22].slice-shell {
  position: relative;
  width: 100%;
  height: 100%;
  box-sizing: border-box;
  overflow: hidden;
  border: 1px solid var(--rr-separator);
  border-radius: 7px;
  background: var(--rr-bg);
  color: var(--rr-text);
}
.slice-header[data-v-syrinx-v22] {
  display: flex;
  gap: 12px;
  align-items: center;
  height: 34px;
  padding: 0 10px;
  border-bottom: 1px solid var(--rr-separator);
  background: var(--rr-panel-bg);
}
.slice-header .cursor { color: var(--rr-text-subdued); font-family: monospace; }
.slice-header .command { margin-left: auto; color: var(--rr-accent); }
.slice-scroll[data-v-syrinx-v22] {
  height: calc(100% - 51px);
  overflow: auto;
  overscroll-behavior: contain;
}
.slice-outside[data-v-syrinx-v22] {
  height: 16px;
  border-top: 1px solid var(--rr-separator);
  background: color-mix(in srgb, var(--rr-panel-bg), transparent 35%);
}
#slice-table[data-v-syrinx-v22] { width: 100%; border-collapse: collapse; table-layout: fixed; }
#slice-table th { position: sticky; top: 0; z-index: 2; text-align: left; background: var(--rr-panel-bg); }
#slice-table th, #slice-table td { padding: 8px 10px; border-bottom: 1px solid var(--rr-separator); }
.slice-row:nth-child(even) { background: color-mix(in srgb, var(--rr-bg), var(--rr-panel-bg) 35%); }
.slice-row.pending { box-shadow: inset 3px 0 var(--rr-accent); }
.slice-row.rejected { box-shadow: inset 3px 0 var(--rr-warning); }
.slice-cell {
  position: relative;
  width: 100%;
  min-height: 30px;
  text-align: left;
  color: inherit;
  background: transparent;
  border: 1px solid transparent;
  border-radius: 4px;
}
.slice-cell:hover, .slice-cell:focus {
  border-color: var(--rr-accent);
  background:
    radial-gradient(circle at 18% 35%, #fff 0 1px, transparent 2px),
    radial-gradient(circle at 72% 28%, #ffd86b 0 1px, transparent 2px),
    radial-gradient(circle at 51% 74%, #fff 0 1px, transparent 2px),
    var(--rr-hover);
  animation: syrinx-sparkle 520ms ease-in-out infinite alternate;
}
@keyframes syrinx-sparkle {
  from { filter: brightness(0.96); background-position: 0 0, 0 0, 0 0; }
  to { filter: brightness(1.18); background-position: 7px -3px, -5px 4px, 3px 6px; }
}
.pending { padding-left: 8px; color: var(--rr-accent); font-size: 0.85em; }
.row-status { color: var(--rr-text-subdued); }
.slice-menu[data-v-syrinx-v22] {
  position: absolute;
  z-index: 10;
  min-width: 210px;
  padding: 10px;
  border: 1px solid var(--rr-accent);
  border-radius: 6px;
  background: var(--rr-panel-bg);
  box-shadow: 0 8px 28px rgba(0, 0, 0, 0.35);
}
.slice-editor-label { display: grid; gap: 4px; margin-top: 7px; color: var(--rr-text-subdued); }
#slice-editor[data-v-syrinx-v22] {
  min-height: 30px;
  box-sizing: border-box;
  color: var(--rr-text);
  border: 1px solid var(--rr-separator);
  border-radius: 4px;
  background: var(--rr-bg);
}
#slice-editor[data-v-syrinx-v22]:focus { border-color: var(--rr-accent); }
.slice-choices { display: grid; grid-template-columns: 1fr 1fr; gap: 6px; margin-top: 7px; }
.slice-choices button {
  min-height: 30px;
  color: var(--rr-text);
  border: 1px solid var(--rr-separator);
  border-radius: 4px;
  background: var(--rr-bg);
}
.slice-choices button:hover, .slice-choices button:focus { border-color: var(--rr-accent); background: var(--rr-hover); }
</style>
