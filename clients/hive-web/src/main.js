import { createApp, shallowRef } from 'vue';
import OpenVue from 'openvue/config';
import Aura from '@openvue/themes/aura';
import { connect } from './client.js';
import App from './App.vue';
import './style.css';

// Replace snapshots as a unit; the transport, not Vue proxies, owns protocol state.
const state = shallowRef({});
const client = connect(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/acp`, snapshot => { state.value = snapshot; });
state.value = client.state();
createApp(App, { client, state }).use(OpenVue, { theme: { preset: Aura } }).mount('#app');
