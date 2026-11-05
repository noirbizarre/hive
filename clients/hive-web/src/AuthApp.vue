<script setup>
import { onBeforeUnmount, onMounted, ref, shallowRef } from 'vue';
import App from './App.vue';
import { connect } from './client.js';

const state = shallowRef({}), client = shallowRef();
const ready = ref(false), required = ref(false), token = ref(''), error = ref(''), busy = ref(false);
let checking;
let generation = 0;
function invalidate() { generation++; checking = undefined; }
function detach() { ready.value = false; client.value?.close(); client.value = undefined; }
async function check(force = false) {
  if (busy.value && !force) return false;
  if (checking) return checking;
  const epoch = generation;
  const task = (async () => {
    try {
      const response = await fetch('/v1/auth', { cache: 'no-store' });
      if (!response.ok) throw new Error('Cannot check authentication; retry when Hive is available');
      const auth = await response.json();
      // A response evaluated before login/logout must never restore the old cookie's state.
      if (epoch !== generation) return false;
      error.value = '';
      required.value = auth.required;
      if (!auth.authenticated) {
        detach();
      } else if (!client.value) {
        // Do not allocate a socket or catalog stream before the cookie is accepted.
        client.value = connect(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/acp`, snapshot => { state.value = snapshot; }, () => check());
        state.value = client.value.state();
        ready.value = true;
      }
      return auth.authenticated;
    } catch (failure) { if (epoch === generation) error.value = failure.message; return false; }
  })();
  checking = task;
  try { return await task; } finally { if (checking === task) checking = undefined; }
}
async function login() {
  busy.value = true; error.value = '';
  invalidate();
  try {
    const response = await fetch('/v1/auth/login', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ token: token.value }) });
    token.value = '';
    if (!response.ok) throw new Error(response.status === 401 ? 'Token not accepted' : 'Sign in failed; check the listener origin and retry');
    invalidate();
    await check(true);
  } catch (failure) { error.value = failure.message; }
  finally { token.value = ''; busy.value = false; }
}
async function logout() {
  busy.value = true;
  invalidate();
  try {
    const response = await fetch('/v1/auth/logout', { method: 'POST' });
    if (!response.ok) throw new Error('Sign out failed; retry');
    // Existing streams are not revoked server-side, so successful logout detaches immediately.
    detach();
    invalidate();
    await check(true);
  } catch (failure) { error.value = failure.message; }
  finally { busy.value = false; }
}
let timer;
onMounted(() => { void check(); timer = setInterval(check, 30000); });
onBeforeUnmount(() => { invalidate(); clearInterval(timer); client.value?.close(); });
</script>

<template>
  <template v-if="ready">
    <button v-if="required" class="sign-out" @click="logout">Sign out</button>
    <p v-if="error" role="alert">{{ error }}</p>
    <App :client="client" :state="state" />
  </template>
  <main v-else class="login-shell">
    <h1>Hive</h1>
    <form v-if="required" @submit.prevent="login">
      <label for="auth-token">Access token</label>
      <input id="auth-token" v-model="token" type="password" autocomplete="off" required :disabled="busy">
      <button type="submit" :disabled="busy">Sign in</button>
    </form>
    <p v-else>Connecting to Hive…</p>
    <p v-if="error" role="alert">{{ error }}</p>
    <button v-if="error && !required" @click="check">Retry</button>
  </main>
</template>

<style scoped>
.login-shell { max-width: 28rem; margin: 10vh auto; padding: 2rem; }
form { display: grid; gap: 1rem; }
input, button { font: inherit; padding: .5rem; }
.sign-out { position: fixed; right: 1rem; bottom: 1rem; z-index: 10; }
</style>
