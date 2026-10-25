import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { flushPromises, mount } from '@vue/test-utils';
import { nextTick, shallowRef } from 'vue';
import OpenVue from 'openvue/config';
import Select from 'openvue/select';
import App from './App.vue';
import InteractionCard from './InteractionCard.vue';
import TranscriptView from './TranscriptView.vue';
import SessionNavigation from './SessionNavigation.vue';

const session = { sessionId: 'hive-tui', cwd: '/workspace', agent: 'pi', title: 'Review parser', activity: 'idle', controlOwner: 'other', generation: 3 };
const options = [{ configId: 'model', name: 'Model', category: 'model', type: 'select', currentValue: 'fast', options: [{ name: 'Provider', options: [{ value: 'fast', name: 'Fast' }, { value: 'careful', name: 'Careful' }] }] }];
let wrapper, source, items, revision;
function render(component, props) {
  wrapper = mount(component, { props, attachTo: document.body, global: { plugins: [[OpenVue, { unstyled: true }]] } });
  return wrapper;
}
function application(initial = {}) {
  const state = shallowRef({ connected: true, status: 'Connected to ACPv2', role: 'observer', lines: [], usage: {}, configOptions: [], ...initial });
  const publish = change => { state.value = { ...state.value, ...change }; };
  const client = {
    open: vi.fn(async target => publish({ session: target, role: 'observer' })),
    create: vi.fn(async agent => publish({ session: { ...session, agent }, role: 'pilot', configOptions: options })),
    takeControl: vi.fn(async () => publish({ role: 'pilot' })),
    configure: vi.fn(async (_, value) => publish({ configOptions: [{ ...options[0], currentValue: value }] })),
    prompt: vi.fn(async () => {}), cancel: vi.fn(), respond: vi.fn(() => true), close: vi.fn(),
    attachWithoutHistory: vi.fn(), acknowledgeRecovery: vi.fn(),
  };
  render(App, { state, client });
  return { state, client, publish };
}
beforeEach(() => {
  history.replaceState(null, '', '/ui');
  items = [session]; revision = 1;
  vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: false, addEventListener() {}, removeEventListener() {} })));
  vi.stubGlobal('fetch', vi.fn(async url => ({ ok: true, json: async () => url.includes('providers') ? { agents: [{ id: 'pi', name: 'Pi' }] } : { revision, sessions: items } })));
  vi.stubGlobal('EventSource', class {
    constructor(url) { source = this; this.url = url; }
    addEventListener(_, callback) { this.revision = callback; }
    close = vi.fn();
  });
});
afterEach(() => { wrapper?.unmount(); document.body.innerHTML = ''; vi.unstubAllGlobals(); });

test('session selection observes the original session and typing requests control before send', async () => {
  const { client } = application();
  await flushPromises();
  expect(source.url).toBe('/v1/sessions/events');
  expect(wrapper.get('.brand img').attributes('src')).toBe('/ui/icon.svg');
  await wrapper.get('[data-session-id="hive-tui"]').trigger('click');
  await flushPromises();
  expect(client.open).toHaveBeenCalledWith(session);
  expect(wrapper.text()).toContain('Read-only');
  expect(wrapper.get('#send').element.disabled).toBe(true);
  await wrapper.get('#composer').setValue('follow up');
  await flushPromises();
  expect(client.takeControl).toHaveBeenCalledOnce();
  expect(wrapper.get('#send').element.disabled).toBe(false);
  await wrapper.get('#prompt').trigger('submit');
  await flushPromises();
  expect(client.prompt).toHaveBeenCalledWith('follow up', 'pi');
  expect(wrapper.get('#composer').element.value).toBe('');
});

test('partial history warnings persist through transport updates without blocking available content', async () => {
  const { publish } = application({ session, role: 'pilot', lines: [{ kind: 'agent', text: 'Retained answer' }],
    transcript: { completeness: 'partial', warnings: [{ code: 'hive::transcript::truncated', message: 'Earlier content remains with the agent' }] } });
  await flushPromises();
  expect(wrapper.get('#history-status').text()).toContain('History: partial');
  expect(wrapper.get('#history-status').text()).toContain('Earlier content remains with the agent');
  expect(wrapper.get('#output').text()).toContain('Retained answer');
  await wrapper.get('#composer').setValue('follow up');
  expect(wrapper.get('#send').element.disabled).toBe(false);
  publish({ status: 'Prompt sent' }); await nextTick();
  expect(wrapper.get('#history-status').text()).toContain('Earlier content remains with the agent');
  publish({ session: { ...session, sessionId: 'other' }, transcript: undefined }); await nextTick();
  expect(wrapper.get('#history-status').text()).toContain('retention coverage unknown');
  expect(wrapper.get('#history-status').text()).not.toContain('Earlier content');
});

test('a fresh session configures grouped models before its first prompt and gates busy controls', async () => {
  const { client, publish } = application();
  await flushPromises();
  expect(wrapper.get('#send').element.disabled).toBe(true);
  await wrapper.get('#create').trigger('click');
  await flushPromises();
  expect(client.prompt).not.toHaveBeenCalled();
  const select = wrapper.findAllComponents(Select).find(item => item.props('inputId') === 'config-model');
  expect(select.props('options')[0]).toMatchObject({ name: 'Provider', items: options[0].options[0].options });
  select.vm.$emit('update:modelValue', 'careful');
  await flushPromises();
  expect(client.configure).toHaveBeenCalledWith('model', 'careful');
  publish({ configuring: true }); await nextTick();
  expect(select.props('disabled')).toBe(true);
  expect(wrapper.get('#send').element.disabled).toBe(true);
});

test('deep links and catalog revisions preserve selection focus and recent ordering', async () => {
  history.replaceState(null, '', '/ui?session=hive-tui');
  items.push({ ...session, sessionId: 'second', title: 'Second session' });
  const { client } = application();
  await flushPromises();
  expect(client.open).toHaveBeenCalledWith(session);
  await wrapper.get('[data-session-id="second"]').trigger('click');
  await flushPromises();
  expect(location.search).toBe('?session=second');
  const button = wrapper.get('[data-session-id="second"]').element;
  button.focus();
  revision++; items = items.map(item => ({ ...item, activity: 'running' }));
  source.revision({ data: String(revision) });
  await flushPromises();
  expect(document.activeElement).toBe(button);
  expect(wrapper.get('#sessions button').attributes('data-session-id')).toBe('second');
  expect(wrapper.get('[aria-current="page"]').text()).toContain('running');
  await wrapper.get('#toggle-sessions').trigger('click');
  expect(wrapper.get('#layout').classes()).toContain('nav-hidden');
});

test('provider invalidations preserve selection and drafts without changing the attached conversation', async () => {
  let agents = [{ id: 'pi', name: 'Pi' }, { id: 'other', name: 'Other' }];
  fetch.mockImplementation(async url => ({ ok: true, json: async () => url.includes('providers')
    ? { agents, defaultAgent: agents[0]?.id } : { revision, sessions: items } }));
  const { client } = application(); await flushPromises();
  const select = wrapper.findAllComponents(Select).find(item => item.props('inputId') === 'agent');
  select.vm.$emit('update:modelValue', 'other');
  await wrapper.get('#composer').setValue('unsent draft');
  agents = [{ id: 'new', name: 'New' }, { id: 'other', name: 'Renamed' }];
  source.revision({ data: String(++revision) }); await flushPromises();
  expect(select.props('modelValue')).toBe('other');
  expect(select.props('options')).toContainEqual({ id: 'other', name: 'Renamed' });
  agents = [{ id: 'new', name: 'New' }];
  source.revision({ data: String(++revision) }); await flushPromises();
  expect(select.props('modelValue')).toBe('new');
  await wrapper.get('[data-session-id="hive-tui"]').trigger('click'); await flushPromises();
  source.revision({ data: String(++revision) }); await flushPromises();
  expect(wrapper.get('#composer').element.value).toBe('unsent draft');
  expect(client.open).toHaveBeenCalledOnce();
  expect(client.create).not.toHaveBeenCalled();
  expect(client.prompt).not.toHaveBeenCalled();
});

test('streaming updates preserve composer drafts focus and open tools without rendering agent HTML', async () => {
  const { publish } = application({ session, role: 'pilot', lines: [{ kind: 'tool', text: '<script>bad()</script>' }] });
  await flushPromises();
  const input = wrapper.get('#composer');
  await input.setValue('draft'); input.element.focus();
  wrapper.get('.tool-message').element.open = true;
  publish({ lines: [{ kind: 'tool', text: '<script>bad()</script> more' }, { kind: 'agent', text: '<b>plain text</b>' }] });
  await flushPromises();
  expect(document.activeElement).toBe(input.element);
  expect(input.element.value).toBe('draft');
  expect(wrapper.get('.tool-message').element.open).toBe(true);
  expect(wrapper.find('#output script').exists()).toBe(false);
  expect(wrapper.get('.message-text').text()).toBe('<b>plain text</b>');
});

test('catalog and backend failures expose retries and empty lists explain creation', async () => {
  fetch.mockResolvedValue({ ok: false, status: 503 });
  application(); await flushPromises();
  expect(wrapper.text()).toContain('Session list failed (503)');
  expect(wrapper.text()).toContain('Agent inventory unavailable (503)');
  items = [];
  fetch.mockImplementation(async url => ({ ok: true, json: async () => url.includes('providers') ? { agents: [] } : { revision: 2, sessions: items } }));
  await wrapper.findAll('button').find(button => button.text() === 'Retry session list').trigger('click');
  await flushPromises();
  expect(wrapper.text()).toContain('No sessions yet. Create a session');
});

test('permission forms preserve typed values and drafts until their fenced request changes', async () => {
  const interaction = { id: 'question', epoch: 1, generation: 3, method: 'elicitation/create', params: { mode: 'form', message: 'Choose', requestedSchema: {
    properties: { approved: { type: 'boolean' }, count: { type: 'integer', minimum: 2 }, empty: { type: 'string' }, values: { type: 'array', items: { type: 'string', enum: ['one', 'two'] } } }, required: ['approved', 'count'],
  } } };
  const { client, publish } = application({ session, role: 'pilot', interaction }); await flushPromises();
  await wrapper.get('[name="approved"]').setValue('false');
  await wrapper.get('[name="count"]').setValue('1');
  await wrapper.get('#interaction form').trigger('submit');
  expect(wrapper.text()).toContain('outside the requested bounds');
  await wrapper.get('[name="count"]').setValue('4');
  await wrapper.get('[type="checkbox"]').setValue(true);
  await wrapper.get('[name="values"]').setValue(['two']);
  publish({ lines: [{ kind: 'agent', text: 'streaming' }] }); await nextTick();
  expect(wrapper.get('[name="count"]').element.value).toBe('4');
  await wrapper.get('#interaction form').trigger('submit');
  expect(client.respond).toHaveBeenCalledWith({ action: 'accept', content: { approved: false, count: 4, empty: '', values: ['two'] } }, 'question');
  publish({ role: 'observer', interaction: undefined }); await nextTick();
  expect(wrapper.find('#interaction').exists()).toBe(false);
});

test('request scoped forms render before creation and URL consent remains distinct from completion', async () => {
  const { client, publish } = application({ interaction: { id: 1, epoch: 1, requestScoped: true, method: 'elicitation/create', params: { mode: 'form', requestedSchema: { properties: { flag: { type: 'boolean', default: false } }, required: ['flag'] } } } });
  await flushPromises();
  await wrapper.get('#interaction form').trigger('submit');
  expect(client.respond).toHaveBeenCalledWith({ action: 'accept', content: { flag: false } }, 1);
  publish({ role: 'pilot', session, interaction: { id: 2, epoch: 1, generation: 3, method: 'elicitation/create', params: { mode: 'url', url: 'https://example.org/auth' } }, urlFlows: [{ id: 'flow', message: 'Authenticate', expires: Date.now() + 10000 }] });
  await nextTick();
  expect(wrapper.get('#interaction a').attributes('rel')).toBe('noopener noreferrer');
  await wrapper.findAll('#interaction button').find(button => button.text() === 'Continue').trigger('click');
  expect(client.respond).toHaveBeenLastCalledWith({ action: 'accept' }, 2);
  expect(wrapper.text()).toContain('Waiting for external completion');
});

test('reconnect recovery and expired history actions never silently send a prompt', async () => {
  const { client, publish } = application({ session, role: 'observer', connected: false, uncertain: { text: 'original' } });
  await flushPromises(); expect(client.prompt).not.toHaveBeenCalled();
  expect(wrapper.get('#send').element.disabled).toBe(true);
  publish({ connected: true, role: 'pilot' }); await nextTick();
  await wrapper.findAll('button').find(button => button.text() === 'Retry original prompt').trigger('click');
  await flushPromises(); expect(client.prompt).toHaveBeenCalledWith('original', 'pi');
  publish({ gap: true, uncertain: undefined }); await nextTick();
  await wrapper.findAll('button').find(button => button.text() === 'Attach without earlier history').trigger('click');
  expect(client.attachWithoutHistory).toHaveBeenCalledOnce();
  publish({ gap: false, recoveryRequired: true }); await nextTick();
  await wrapper.findAll('button').find(button => button.text() === 'Acknowledge recovery').trigger('click');
  expect(client.acknowledgeRecovery).toHaveBeenCalledOnce();
});

test('session search filters derived catalog entries without changing control ownership', async () => {
  render(SessionNavigation, { sessions: [session], state: { session, role: 'pilot', connected: true, generation: 2 }, recent: [], loading: false });
  expect(wrapper.text()).toContain('Controlled by another client');
  await wrapper.get('input').setValue('missing');
  expect(wrapper.text()).toContain('No sessions match');
  expect(wrapper.emitted('open')).toBeUndefined();
});

test('scrolling away from the bottom preserves reading position until jumping to latest', async () => {
  render(TranscriptView, { state: { session, lines: [{ kind: 'agent', text: 'First' }], usage: {} } });
  const element = wrapper.get('#output').element;
  Object.defineProperties(element, { scrollHeight: { value: 1200 }, clientHeight: { value: 400 } });
  element.scrollTop = 100; await wrapper.get('#output').trigger('scroll');
  await wrapper.setProps({ state: { session, lines: [{ kind: 'agent', text: 'First more' }], usage: {} } });
  await flushPromises();
  expect(element.scrollTop).toBe(100);
  await wrapper.get('.jump-latest').trigger('click');
  expect(element.scrollTop).toBe(1200);
  expect(wrapper.text()).toContain('Cost unknown');
});

test('withdrawn permission requests return focus and unsafe URLs never become links', async () => {
  const button = document.createElement('button'); document.body.append(button); button.focus();
  render(InteractionCard, { interaction: { id: 1, method: 'elicitation/create', params: { mode: 'url', url: 'javascript:alert(1)' } }, client: { respond: vi.fn() } });
  expect(wrapper.find('a').exists()).toBe(false);
  wrapper.unmount(); wrapper = null;
  expect(document.activeElement).toBe(button);
});
