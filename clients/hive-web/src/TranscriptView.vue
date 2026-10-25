<script setup>
import { computed, nextTick, ref, watch } from 'vue';
import Button from 'openvue/button';
const props = defineProps({ state: { type: Object, required: true } });
const viewport = ref();
const following = ref(true);
const hasNew = ref(false);
const cost = computed(() => {
  const usage = props.state.usage || {};
  const actual = usage.actualSessionCost;
  const estimate = usage.estimatedCost;
  return actual?.amount != null ? `Actual ${actual.amount.toFixed(4)} ${actual.currency}`
    : estimate?.amount != null ? `Estimated ${estimate.amount.toFixed(4)} ${estimate.currency}` : 'Cost unknown';
});
function scroll() {
  const element = viewport.value;
  following.value = element.scrollHeight - element.scrollTop - element.clientHeight < 80;
  if (following.value) hasNew.value = false;
}
function latest() { viewport.value.scrollTop = viewport.value.scrollHeight; following.value = true; hasNew.value = false; }
watch(() => props.state, async (state, previous) => {
  const switched = state.session?.sessionId !== previous?.session?.sessionId;
  await nextTick();
  if (switched || following.value) latest();
  else hasNew.value = true;
});
</script>

<template>
  <div class="transcript-container">
    <section id="output" ref="viewport" class="transcript" aria-label="Conversation" tabindex="0" @scroll="scroll">
      <div v-if="!state.lines?.length" class="welcome">
        <img src="/ui/icon.svg" alt="" width="56" height="56">
        <h2>{{ state.opening ? 'Opening conversation…' : state.session ? 'Ready when you are' : 'A home for your agent sessions' }}</h2>
        <p>{{ state.session ? 'Your agent’s retained conversation appears here. Take control to continue.' : 'Choose a session to catch up, or create one to start something new.' }}</p>
        <div class="welcome-hint">One session. Every client. Always in sync.</div>
      </div>
      <!-- Reducer lines only append within a session; indices preserve open tools during chunks. -->
      <template v-for="(line, index) in state.lines" :key="`${state.session?.sessionId}:${index}`">
        <details v-if="line.kind === 'tool'" class="tool-message"><summary>{{ line.title || 'Tool activity' }} <span v-if="line.status">· {{ line.status }}</span></summary><pre v-if="line.input">{{ line.input }}</pre><pre v-if="line.text">{{ line.text }}</pre><p v-else class="muted">No output reported</p></details>
        <article v-else class="message" :class="line.kind">
          <div class="message-speaker">{{ line.kind === 'user' ? 'You' : line.kind === 'agent' ? 'Agent' : 'Session notice' }}</div>
          <div class="message-text">{{ line.text }}</div>
        </article>
      </template>
      <details v-if="state.session" class="activity-panel">
        <summary>Activity · {{ state.activity?.session || 'active' }} / {{ state.activity?.foreground || 'unknown' }}</summary>
        <div v-for="[label, key] in [['Tools', 'tools'], ['Tasks', 'tasks'], ['Todos', 'plans']]" :key="key">
          <h3>{{ label }} <small>{{ state.activity?.support?.[key] || 'unknown' }}</small></h3>
          <p v-if="!Object.keys(state.activity?.[key] || {}).length" class="muted">No reported {{ label.toLowerCase() }}</p>
          <div v-for="(item, id) in state.activity?.[key] || {}" :key="id" class="activity-item">
            <template v-if="key === 'plans'"><p v-for="(entry, i) in item.entries || []" :key="i">{{ entry.status }} · {{ entry.content }}</p></template>
            <template v-else>{{ item.title || id }} · {{ item.status || 'unknown' }}</template>
          </div>
        </div>
      </details>
      <p v-for="flow in state.urlFlows || []" :key="flow.id" class="flow-status" role="status">{{ flow.message }}: {{ flow.complete ? 'Completed' : flow.expires <= Date.now() ? 'Completion not received before timeout' : 'Waiting for external completion' }}</p>
    </section>
    <Button v-if="hasNew" class="jump-latest" label="Jump to latest ↓" size="small" @click="latest" />
    <div v-if="state.session" class="usage" aria-label="Usage">{{ state.usage?.tokens?.totalTokens ?? 'Unknown' }} tokens <span>·</span> {{ cost }} <span>·</span> Reported turns {{ state.usage?.reportedTurns ?? 0 }} / {{ state.usage?.turns ?? 0 }}</div>
  </div>
</template>
