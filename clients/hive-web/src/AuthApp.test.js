import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { flushPromises, mount } from '@vue/test-utils';
import AuthApp from './AuthApp.vue';
import { connect } from './client.js';

vi.mock('./client.js', () => ({ connect: vi.fn() }));
vi.mock('./App.vue', () => ({ default: { props: ['client', 'state'], template: '<div>Connected application</div>' } }));
let wrapper, authenticated, required, close, disconnected;
beforeEach(() => {
  authenticated = false; required = true; close = vi.fn();
  connect.mockImplementation((url, update, onDisconnect) => { disconnected = onDisconnect; return { close, state: () => ({ connected: true }) }; });
  vi.stubGlobal('fetch', vi.fn(async (url, options) => {
    if (url.endsWith('/login')) { authenticated = JSON.parse(options.body).token === 'correct'; return { ok: authenticated, status: authenticated ? 200 : 401 }; }
    if (url.endsWith('/logout')) { authenticated = false; return { ok: true }; }
    return { ok: true, json: async () => ({ required, authenticated: authenticated || !required }) };
  }));
});
afterEach(() => { wrapper?.unmount(); vi.useRealTimers(); vi.unstubAllGlobals(); vi.clearAllMocks(); });

test('token login gates all application connections and clears the token after failed and successful attempts', async () => {
  wrapper = mount(AuthApp);
  await flushPromises();
  expect(connect).not.toHaveBeenCalled();
  await wrapper.get('input').setValue('incorrect');
  await wrapper.get('form').trigger('submit');
  await flushPromises();
  expect(wrapper.get('input').element.value).toBe('');
  expect(wrapper.text()).toContain('Token not accepted');
  expect(connect).not.toHaveBeenCalled();
  await wrapper.get('input').setValue('correct');
  await wrapper.get('form').trigger('submit');
  await flushPromises();
  expect(wrapper.text()).toContain('Connected application');
  expect(connect).toHaveBeenCalledOnce();
  expect(connect.mock.calls[0][0]).not.toContain('correct');
  await wrapper.get('button').trigger('click');
  await flushPromises();
  expect(close).toHaveBeenCalled();
  expect(wrapper.find('form').exists()).toBe(true);
});

test('expired browser authentication closes the client before it can reconnect', async () => {
  authenticated = true;
  wrapper = mount(AuthApp);
  await flushPromises();
  authenticated = false;
  await disconnected();
  await flushPromises();
  expect(close).toHaveBeenCalled();
  expect(wrapper.find('form').exists()).toBe(true);
  expect(connect).toHaveBeenCalledOnce();
});

test('authentication disabled connects automatically without presenting a login', async () => {
  required = false;
  wrapper = mount(AuthApp);
  await flushPromises();
  expect(wrapper.find('form').exists()).toBe(false);
  expect(wrapper.text()).toContain('Connected application');
  expect(connect).toHaveBeenCalledOnce();
});

test('a pre-login authentication response cannot undo a successful login', async () => {
  vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] });
  wrapper = mount(AuthApp);
  await flushPromises();
  let stale;
  fetch.mockImplementationOnce(() => new Promise(resolve => { stale = resolve; }));
  await vi.advanceTimersByTimeAsync(30000);
  await wrapper.get('input').setValue('correct');
  await wrapper.get('form').trigger('submit');
  await flushPromises();
  expect(wrapper.text()).toContain('Connected application');
  stale({ ok: true, json: async () => ({ required: true, authenticated: false }) });
  await flushPromises();
  expect(wrapper.text()).toContain('Connected application');
  expect(close).not.toHaveBeenCalled();
});

test('logout detaches immediately and a pre-logout authentication response cannot reopen the application', async () => {
  authenticated = true;
  wrapper = mount(AuthApp);
  await flushPromises();
  let stale;
  fetch.mockImplementationOnce(() => new Promise(resolve => { stale = resolve; }));
  const outstanding = disconnected();
  await flushPromises();
  await wrapper.get('button').trigger('click');
  await flushPromises();
  expect(close).toHaveBeenCalled();
  expect(wrapper.find('form').exists()).toBe(true);
  stale({ ok: true, json: async () => ({ required: true, authenticated: true }) });
  await outstanding;
  await flushPromises();
  expect(wrapper.find('form').exists()).toBe(true);
  expect(connect).toHaveBeenCalledOnce();
});
