<script setup>
import { computed, onMounted, onBeforeUnmount, ref } from 'vue';
import Button from 'openvue/button';
import Message from 'openvue/message';
import { formValues } from './client.js';

const props = defineProps({ interaction: { type: Object, required: true }, client: { type: Object, required: true } });
const params = computed(() => props.interaction.params);
const schema = computed(() => params.value.requestedSchema || {});
const error = ref('');
const root = ref();
const previousFocus = document.activeElement;
const choices = field => field.enum?.map(value => ({ const: value, title: value })) || field.oneOf || field.items?.enum?.map(value => ({ const: value, title: value })) || field.items?.anyOf;
const safeUrl = computed(() => {
  try { const url = new URL(params.value.url); return ['http:', 'https:'].includes(url.protocol) ? url.href : null; }
  catch { return null; }
});
function respond(result) {
  try {
    if (!props.client.respond(result, props.interaction.id)) error.value = 'This request is no longer active. Wait for the current request.';
  } catch (failure) { error.value = failure.message; }
}
function submit(event) {
  try { respond({ action: 'accept', content: formValues(schema.value, event.target) }); }
  catch (failure) { error.value = failure.message; }
}
onMounted(() => {
  // Apply defaults once per fenced request, never on each streaming update.
  for (const [name, field] of Object.entries(schema.value.properties || {})) {
    const input = root.value.querySelector('form')?.elements.namedItem(name);
    if (input && field.default != null) {
      if (input.multiple) for (const option of input.options) option.selected = field.default.includes(option.value);
      else input.value = String(field.default);
    }
  }
  root.value.querySelector('input, select, button, a')?.focus();
});
onBeforeUnmount(() => { if (root.value?.contains(document.activeElement) && previousFocus?.isConnected) previousFocus.focus(); });
</script>

<template>
  <section id="interaction" ref="root" class="interaction surface" aria-labelledby="interaction-title">
    <div class="eyebrow">Your attention is needed</div>
    <h2 id="interaction-title">{{ params.title || params.message || 'Agent needs input' }}</h2>
    <p v-if="params.description" class="interaction-description">{{ params.description }}</p>
    <pre v-if="params.subject">{{ JSON.stringify(params.subject, null, 2) }}</pre>
    <Message v-if="error" severity="error" :closable="false" role="alert">{{ error }}</Message>
    <form v-if="interaction.method === 'session/request_permission'" class="actions" @submit.prevent>
      <Button v-for="option in params.options || []" :key="option.optionId" type="button"
        :label="`${option.name}${option.kind ? ` (${option.kind})` : ''}`"
        @click="respond({ outcome: { outcome: 'selected', optionId: option.optionId } })" />
      <Button type="button" label="Decline" severity="secondary" outlined @click="respond({ outcome: { outcome: 'cancelled' } })" />
    </form>
    <form v-else-if="params.mode === 'form'" novalidate @submit.prevent="submit">
      <h3 v-if="schema.title">{{ schema.title }}</h3>
      <p v-if="schema.description">{{ schema.description }}</p>
      <div v-for="(field, name, index) in schema.properties" :key="name" class="request-field">
        <label :for="`field-${index}`">{{ field.title || name }}{{ schema.required?.includes(name) ? ' (required)' : '' }}</label>
        <select v-if="field.type === 'boolean'" :id="`field-${index}`" :name="name" :aria-required="Boolean(schema.required?.includes(name))">
          <option value="">Choose…</option><option value="true">Yes</option><option value="false">No</option>
        </select>
        <select v-else-if="choices(field)" :id="`field-${index}`" :name="name" :multiple="field.type === 'array'" :aria-required="Boolean(schema.required?.includes(name))">
          <option v-if="field.type !== 'array'" value="">Choose…</option>
          <option v-for="option in choices(field)" :key="option.const" :value="option.const">{{ option.title || option.const }}</option>
        </select>
        <input v-else :id="`field-${index}`" :name="name" :type="['integer', 'number'].includes(field.type) ? 'number' : 'text'"
          :step="field.type === 'integer' ? '1' : 'any'" :min="field.minimum" :max="field.maximum" :aria-required="Boolean(schema.required?.includes(name))">
        <small v-if="field.description">{{ field.description }}</small>
        <label v-if="field.type === 'string' && !schema.required?.includes(name)" class="inline-label">
          <input type="checkbox" @change="event => { root.querySelector('form').elements.namedItem(name).dataset.includeEmpty = String(event.target.checked); }"> Include empty value
        </label>
        <details><summary>Constraints for {{ field.title || name }}</summary><pre>{{ JSON.stringify(field, null, 2) }}</pre></details>
      </div>
      <div class="actions"><Button type="submit" label="Submit" /><Button type="button" label="Decline" severity="secondary" outlined @click="respond({ action: 'decline' })" /></div>
    </form>
    <div v-else-if="params.mode === 'url'">
      <a v-if="safeUrl" :href="safeUrl" target="_blank" rel="noopener noreferrer">{{ safeUrl }}</a>
      <p>Continue to allow the external flow. Completion is reported separately by the agent.</p>
      <div class="actions"><Button label="Continue" :disabled="!safeUrl" @click="respond({ action: 'accept' })" /><Button label="Decline" severity="secondary" outlined @click="respond({ action: 'decline' })" /></div>
    </div>
  </section>
</template>

<style scoped>
/* Preserve the boundaries between provider requests and plan excerpts. */
.interaction-description { white-space: pre-wrap; overflow-wrap: anywhere; }
</style>
