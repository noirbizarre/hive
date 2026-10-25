<script setup>
import { computed, ref } from 'vue';
import InputText from 'openvue/inputtext';
import Button from 'openvue/button';
const props = defineProps({ sessions: Array, state: Object, recent: Array, loading: Boolean, error: String, listId: { type: String, default: 'sessions' } });
defineEmits(['open', 'refresh']);
const search = ref('');
const ordered = computed(() => [...props.sessions].filter(session => [session.title, session.sessionId, session.agent, session.cwd].join(' ').toLowerCase().includes(search.value.toLowerCase())).sort((a, b) => {
  const first = props.recent.indexOf(a.sessionId), second = props.recent.indexOf(b.sessionId);
  if (first !== -1 || second !== -1) return (first === -1 ? Infinity : first) - (second === -1 ? Infinity : second);
  return (b.lastActivityAt || 0) - (a.lastActivityAt || 0) || a.sessionId.localeCompare(b.sessionId);
}));
function ownership(session) {
  if (!session.controlOwner) return 'Unclaimed';
  return session.sessionId === props.state.session?.sessionId && props.state.role === 'pilot' && props.state.connected && props.state.generation === session.generation ? 'Controlled here' : 'Controlled by another client';
}
</script>

<template>
  <nav aria-label="Sessions" class="session-navigation">
    <div class="rail-heading"><h2>Sessions</h2><span class="count">{{ sessions.length }}</span></div>
    <InputText v-model="search" type="search" aria-label="Search sessions" placeholder="Search sessions…" class="session-search" />
    <p v-if="loading" role="status" class="rail-state">Loading sessions…</p>
    <div v-else-if="error" class="rail-state" role="alert"><p>{{ error }}</p><Button label="Retry session list" size="small" severity="secondary" @click="$emit('refresh')" /></div>
    <p v-else-if="!sessions.length" class="rail-state">No sessions yet. Create a session to get started.</p>
    <p v-else-if="!ordered.length" class="rail-state">No sessions match your search.</p>
    <ul :id="listId" class="session-list">
      <li v-for="session in ordered" :key="session.sessionId">
        <button type="button" :data-session-id="session.sessionId" :aria-current="session.sessionId === state.session?.sessionId ? 'page' : undefined" @click="$emit('open', session)">
          <span class="session-title">{{ session.title || session.sessionId }}</span>
          <span class="session-meta">{{ session.agent || 'Unknown backend' }} <span>·</span> {{ session.lifecycle && session.lifecycle !== 'active' ? session.lifecycle : session.activity || 'unknown' }}</span>
          <span v-if="session.sessionId === state.session?.sessionId && state.interaction" class="attention">Needs attention</span>
          <span class="session-owner">{{ ownership(session) }}</span>
          <span class="session-id">{{ session.sessionId }}</span>
          <span v-if="session.recoveryReason" class="attention">{{ session.recoveryReason }}</span>
          <time v-if="session.lastActivityAt" :datetime="new Date(session.lastActivityAt).toISOString()">{{ new Date(session.lastActivityAt).toLocaleString() }}</time>
          <span v-else class="session-id">No activity yet</span>
        </button>
      </li>
    </ul>
    <p class="rail-footer">Sessions are managed by your Hive daemon.</p>
  </nav>
</template>
