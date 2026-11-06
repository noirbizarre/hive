<script setup>
import { computed, nextTick, onBeforeUnmount, onMounted, ref, unref, watch } from 'vue';
import Button from 'openvue/button';
import Select from 'openvue/select';
import Textarea from 'openvue/textarea';
import Tag from 'openvue/tag';
import Message from 'openvue/message';
import Drawer from 'openvue/drawer';
import { sessionKey } from './federation.js';
import SessionNavigation from './SessionNavigation.vue';
import TranscriptView from './TranscriptView.vue';
import InteractionCard from './InteractionCard.vue';
import ActivityIndicator from './ActivityIndicator.vue';

const props = defineProps({ client: { type: Object, required: true }, state: { type: Object, required: true } });
const state = computed(() => unref(props.state));
const sessions = ref([]), recent = ref([]), agents = ref([]);
const executionProfiles = ref([]), executionProfile = ref(''), defaultExecutionProfile = ref('');
const owners = ref([]), launchOwner = ref(''), launchCwd = ref('');
const launch = () => ({ owner: launchOwner.value, cwd: launchCwd.value });
const launchProfile = computed(() => executionProfile.value || (launchOwner.value && !owners.value.find(owner => owner.owner === launchOwner.value)?.local ? defaultExecutionProfile.value : ''));
const selectedProfile = computed(() => executionProfiles.value.find(profile => profile.id === (executionProfile.value || defaultExecutionProfile.value)));
const availableAgents = computed(() => selectedProfile.value
  ? agents.value.filter(agent => selectedProfile.value.agents.includes(agent.id)) : agents.value);
const loading = ref(true), listError = ref(''), providerError = ref(''), error = ref('');
const listWarning = ref('');
const drawer = ref(false), navigation = ref(true), text = ref(''), backend = ref(''), sending = ref(false);
const mobileMenu = ref();
const configFailures = ref({});
let feed;
let deepLink = new URLSearchParams(location.search).get('session');
const deepOwner = new URLSearchParams(location.search).get('owner');
const pilot = computed(() => state.value.role === 'pilot' && state.value.connected && !state.value.gap);
const selected = computed(() => sessions.value.find(item => item.sessionId === state.value.session?.sessionId));
const busy = computed(() => state.value.opening || state.value.configuring || state.value.creating || state.value.takingControl || sending.value);
const blocked = computed(() => !state.value.connected ? 'Waiting for the daemon connection'
  : state.value.gap ? 'Attach without earlier history to continue'
  : busy.value ? 'Waiting for the current operation'
  : state.value.recoveryRequired ? 'Inspect agent state and acknowledge recovery first'
  : state.value.uncertain ? 'Retry the unacknowledged prompt first'
  : !state.value.session ? 'Create a session before sending'
   : state.value.running ? (state.value.activity?.foreground === 'requires_action'
     ? 'Agent is waiting for action — respond or stop it' : 'Agent is working — wait or stop it')
  : !pilot.value ? 'Start typing or take control to continue' : '');
const configDisabled = computed(() => !pilot.value || busy.value || state.value.running || state.value.recoveryRequired || Boolean(state.value.uncertain));
const interactionKey = computed(() => JSON.stringify([state.value.interaction?.id, state.value.interaction?.epoch, state.value.interaction?.generation]));
const configOptions = computed(() => (state.value.configOptions || []).filter(option => option.type === 'select'));
// Normalize mixed grouped/ungrouped choices without changing daemon option values.
const groups = option => (option.options || []).map(choice => Array.isArray(choice.options) ? { ...choice, name: choice.name || choice.group, items: choice.options } : { name: option.name, items: [choice] });
const hasChoices = option => groups(option).some(group => group.items.some(item => typeof item.value === 'string'));
// Select keeps an internal optimistic value; its visible label must remain agent-confirmed even on rejection.
const currentLabel = option => groups(option).flatMap(group => group.items).find(item => item.value === option.currentValue)?.name ?? String(option.currentValue ?? 'Unavailable');
async function action(operation) { error.value = ''; try { return await operation(); } catch (failure) { error.value = failure.message; } }
async function configure(option, value) {
  const session = state.value.session?.sessionId;
  await action(async () => {
    try { await props.client.configure(option.configId, value); }
    catch (failure) {
      if (state.value.session?.sessionId === session) {
        // Reset Select's internal selection too, without remounting unrelated controls or the composer.
        const id = `config-${option.configId}`, focused = document.activeElement?.id === id;
        configFailures.value[option.configId] = (configFailures.value[option.configId] || 0) + 1;
        await nextTick();
        if (focused) document.getElementById(id)?.focus();
      }
      throw failure;
    }
  });
}
async function open(session) { drawer.value = false; await action(() => props.client.open(session)); }
function providers({ agents: inventory, defaultAgent, profiles = [], defaultProfile }) {
  providerError.value = '';
  agents.value = inventory;
  executionProfiles.value = profiles;
  defaultExecutionProfile.value = defaultProfile || '';
  // Empty means follow the daemon, not pin today's default profile across reloads.
  if (executionProfile.value && !profiles.some(profile => profile.id === executionProfile.value)) executionProfile.value = '';
  // A configuration reload updates choices without changing the attached session or draft.
  if (!availableAgents.value.some(agent => agent.id === backend.value)) backend.value = selectedProfile.value ? '' : defaultAgent || inventory[0]?.id || '';
}
watch(executionProfile, () => { backend.value = ''; });
watch(launchOwner, owner => {
  backend.value = ''; executionProfile.value = ''; defaultExecutionProfile.value = ''; agents.value = []; executionProfiles.value = []; launchCwd.value = '';
  feed?.selectOwner?.(owner);
});
function typing() { if (text.value && state.value.session && !pilot.value && !state.value.recoveryRequired && !busy.value) action(() => props.client.takeControl()); }
async function send(retry = false) {
  if (sending.value || (!retry && (blocked.value || !text.value.trim()))) return;
  sending.value = true;
  const target = state.value.session?.sessionId;
  const draft = text.value;
  await action(async () => {
    await props.client.prompt(retry ? state.value.uncertain.text : draft, backend.value, launchProfile.value, launch());
    // A delayed acknowledgement must not erase a newer draft or another session's input.
    if (!retry && state.value.session?.sessionId === target && text.value === draft) text.value = '';
  });
  sending.value = false;
}
watch(() => sessionKey(state.value.session), () => {
  const id = state.value.session?.sessionId;
  if (!id) return;
  const key = sessionKey(state.value.session), reference = state.value.session.reference;
  recent.value = [key, ...recent.value.filter(item => item !== key && item !== id)];
  history.replaceState(null, '', `?${new URLSearchParams(reference ? { owner: reference.owner, session: reference.id } : { session: id })}`);
  backend.value = state.value.session.agent || '';
  error.value = '';
});
onMounted(() => {
  feed = props.client.catalog((items, metadata) => {
    sessions.value = items; loading.value = false; listError.value = '';
    listWarning.value = metadata?.partial ? `Session list incomplete — unavailable owners: ${(metadata.unavailableOwners || []).join(', ')}` : '';
    if (deepLink) {
      const found = items.find(item => deepOwner ? item.reference?.owner === deepOwner && item.reference.id === deepLink : item.sessionId === deepLink);
      if (found || deepOwner) open(found || { reference: { owner: deepOwner, id: deepLink } });
      else error.value = `Hive session ${deepLink} not found; choose a listed session`;
      deepLink = null;
    }
  }, message => {
    if (message.startsWith('Agent inventory unavailable')) providerError.value = message;
    else { loading.value = false; listError.value = message; }
   }, providers, items => { owners.value = items; });
});
onBeforeUnmount(() => { feed?.close(); props.client.close(); });
</script>

<template>
  <a class="skip-link" href="#composer">Skip to message input</a>
  <div id="layout" class="workspace" :class="{ 'nav-hidden': !navigation }">
    <aside class="desktop-rail">
      <a class="brand" href="/ui"><img src="/ui/icon.svg" alt="" width="32" height="32"><span>hive<span class="brand-subtitle">Agent workspace</span></span></a>
      <SessionNavigation :sessions="sessions" :state="state" :recent="recent" :loading="loading" :error="listError" @open="open" @refresh="feed.refresh()" />
    </aside>
    <!-- The overlay traps focus, but the application must restore its own trigger. -->
    <Drawer id="session-drawer" v-model:visible="drawer" header="Hive sessions" class="mobile-drawer" role="dialog" @after-hide="mobileMenu?.$el.focus()">
      <SessionNavigation list-id="mobile-sessions" :sessions="sessions" :state="state" :recent="recent" :loading="loading" :error="listError" @open="open" @refresh="feed.refresh()" />
    </Drawer>
    <main class="main-panel">
      <header class="workspace-header">
        <Button ref="mobileMenu" class="mobile-menu" label="Sessions" severity="secondary" outlined aria-controls="session-drawer" :aria-expanded="drawer" @click="drawer = true" />
        <Button id="toggle-sessions" class="desktop-menu" :label="navigation ? 'Hide sessions' : 'Show sessions'" severity="secondary" text :aria-expanded="navigation" @click="navigation = !navigation" />
        <div class="session-heading"><div class="eyebrow">{{ state.session ? selected?.agent || state.session.agent || 'Agent session' : 'Welcome to Hive' }}</div><h1>{{ selected?.title || state.session?.sessionId || 'Your next idea starts here' }}</h1><p v-if="state.session" :title="state.session.cwd">{{ state.session.cwd }}</p></div>
        <div class="connection-state"><Tag :value="state.connected ? 'Connected' : 'Reconnecting'" :severity="state.connected ? 'success' : 'warn'" /><Tag v-if="state.session" :value="state.takingControl ? 'Transferring control' : pilot ? 'Pilot · controlling here' : 'Read-only'" :severity="pilot ? 'info' : 'secondary'" /></div>
        <Button v-if="state.session" as="a" href="/ui" label="New session" size="small" severity="secondary" outlined />
      </header>
      <section class="configuration-bar" aria-label="Session configuration">
        <label v-if="!state.session && owners.length" class="config-field" for="owner">Hive instance<Select v-model="launchOwner" input-id="owner" :options="owners.map(owner => ({ ...owner, label: owner.name || owner.alias || owner.owner }))" option-label="label" option-value="owner" placeholder="Local" :disabled="busy" /></label>
        <label v-if="!state.session && launchOwner && !owners.find(owner => owner.owner === launchOwner)?.local" class="config-field" for="owner-cwd">Working directory on owner<input id="owner-cwd" v-model="launchCwd" placeholder="Absolute owner path (or managed profile)" :disabled="busy"></label>
        <label v-if="executionProfiles.length && !state.session" class="config-field" for="profile">Execution profile<Select v-model="executionProfile" input-id="profile" :options="[{ id: '', name: 'Daemon default' }, ...executionProfiles]" option-label="name" option-value="id" :disabled="busy" /></label>
        <label class="config-field" for="agent">Hive backend<Select :model-value="state.session ? selected?.agent || state.session.agent || backend : backend" input-id="agent" placeholder="Default backend" :options="[{ id: '', name: 'Default backend' }, ...(state.session ? agents : availableAgents)]" option-label="name" option-value="id" :disabled="Boolean(state.session) || busy" @update:model-value="value => backend = value" /></label>
        <Button v-if="!state.session" id="create" label="Create session" :loading="state.connected && state.creating && !state.interaction" :disabled="!state.connected || busy || Boolean(providerError)" @click="action(() => client.create(backend, launchProfile, launch()))" />
        <label v-for="option in configOptions" :key="option.configId" class="config-field" :for="`config-${option.configId}`">{{ option.category === 'model' ? 'Provider / Model' : option.name }}
          <Select :key="configFailures[option.configId] || 0" :input-id="`config-${option.configId}`" :model-value="option.currentValue" :aria-label="currentLabel(option)" :placeholder="String(option.currentValue ?? 'Unavailable')" :title="option.description" :options="groups(option)" option-group-label="name" option-group-children="items" option-label="name" option-value="value" :disabled="configDisabled || !hasChoices(option)" @update:model-value="value => configure(option, value)">
            <template #value><ActivityIndicator v-if="state.connected && state.configuring && !state.interaction" />{{ currentLabel(option) }}</template>
          </Select>
        </label>
        <p v-if="!configOptions.length" class="config-hint">{{ state.session ? 'This backend does not expose session configuration' : 'Create a session to choose its model and settings. You can change them between turns.' }}</p>
        <Button v-if="state.session && !pilot" id="control" label="Take control" severity="secondary" outlined :loading="state.connected && state.takingControl && !state.interaction" :disabled="!state.connected || state.recoveryRequired || busy || state.gap" @click="action(() => client.takeControl())" />
      </section>
      <div class="notices">
        <Message v-if="listWarning" severity="warn" :closable="false" role="status">{{ listWarning }}</Message>
        <!-- Coverage survives transient transport messages and never blocks available content. -->
        <Message v-if="state.session" id="history-status" severity="warn" :closable="false" role="status">
          {{ state.transcript?.completeness === 'partial' ? 'History: partial' : 'History: retention coverage unknown' }}
          <span v-for="warning in state.transcript?.warnings || []" :key="warning.code"> · {{ warning.message }}</span>
        </Message>
        <Message v-if="providerError" severity="error" :closable="false">{{ providerError }} <Button label="Retry backends" size="small" @click="feed.refresh()" /></Message>
        <Message v-if="error" severity="error" :closable="false" role="alert">{{ error }}</Message>
        <Message v-if="state.gap" severity="warn" :closable="false">Earlier history is unavailable. <Button label="Attach without earlier history" :loading="state.connected && state.opening && !state.interaction" :disabled="!state.connected || busy" @click="action(() => client.attachWithoutHistory())" /></Message>
        <Message v-if="state.recoveryRequired" severity="warn" :closable="false">Work was interrupted. Inspect the agent state before continuing. <Button label="Acknowledge recovery" :loading="state.connected && state.opening && !state.interaction" :disabled="!state.connected || busy" @click="action(() => client.acknowledgeRecovery())" /></Message>
        <Message v-else-if="state.uncertain" severity="warn" :closable="false">The prompt has not been acknowledged. Receipt lookup uses its original content and key. <Button label="Retrieve original receipt" :disabled="!pilot || busy || !state.connected" @click="send(true)" /></Message>
      </div>
      <TranscriptView :state="state" />
      <div class="composer-area">
        <InteractionCard v-if="state.interaction && (pilot || state.interaction.requestScoped)" :key="interactionKey" :interaction="state.interaction" :client="client" />
        <form id="prompt" class="composer surface" @submit.prevent="send()">
          <label for="composer" class="sr-only">Message your agent</label>
          <Textarea id="composer" v-model="text" name="text" rows="3" placeholder="What would you like to work on?" :disabled="!state.connected || state.gap || state.opening" aria-describedby="composer-help" @input="typing" />
          <div class="composer-footer"><span id="composer-help"><ActivityIndicator v-if="state.connected && state.opening && !state.interaction" />{{ blocked || 'Ready to send · Your session stays with the daemon' }}</span><div class="actions"><Button id="cancel" :label="state.stopping ? 'Stopping' : 'Stop'" severity="secondary" text :loading="state.connected && state.stopping" :disabled="!pilot || !state.running || state.stopping" @click="client.cancel()" /><Button id="send" type="submit" label="Send message ↑" :loading="state.connected && sending && !state.interaction" :disabled="Boolean(blocked) || !text.trim()" /></div></div>
        </form>
        <p id="status" class="transport-status" role="status" aria-live="polite">{{ state.status }}</p>
      </div>
    </main>
  </div>
</template>
