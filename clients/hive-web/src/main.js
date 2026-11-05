import { createApp } from 'vue';
import OpenVue from 'openvue/config';
import Aura from '@openvue/themes/aura';
import AuthApp from './AuthApp.vue';
import './style.css';

createApp(AuthApp).use(OpenVue, { theme: { preset: Aura } }).mount('#app');
