import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { flushPromises, mount } from '@vue/test-utils';
import { nextTick, shallowRef } from 'vue';
import OpenVue from 'openvue/config';
import Select from 'openvue/select';
import Button from 'openvue/button';
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

test('only executing work spins and historical thinking never reactivates with a later turn', async () => {
  const statuses = ['in_progress', 'pending', 'queued', 'completed', 'failed', 'cancelled', 'future', undefined];
  const state = { connected: true, session, running: true,
    lines: [...statuses.map(status => ({ kind: 'tool', title: 'Read', status })),
      { kind: 'thought', text: 'Old thinking', active: false }, { kind: 'thought', text: 'Live thinking', active: true }],
    activity: { session: 'active', foreground: 'running',
      tasks: Object.fromEntries([...statuses, 'running'].map((status, id) => [id, { status, title: 'Task' }])),
      tools: {}, plans: { plan: { entries: statuses.map(status => ({ status, content: 'Step' })) } } } };
  render(TranscriptView, { state });
  expect(wrapper.findAll('.activity-spinner')).toHaveLength(6);
  expect(wrapper.findAll('.message-speaker').map(node => node.text())).toEqual(['Thinking', 'Thinking']);
  const tool = wrapper.get('.tool-message'); tool.element.open = true;
  await wrapper.setProps({ state: { ...state, activity: { ...state.activity, foreground: 'requires_action' } } });
  expect(wrapper.findAll('.activity-spinner')).toHaveLength(4);
  expect(tool.element.open).toBe(true);
  // Foreground idle leaves reported background tasks and tools independently active.
  await wrapper.setProps({ state: { ...state, lines: state.lines.map(line => ({ ...line, active: false })), running: false, activity: { ...state.activity, foreground: 'idle' } } });
  expect(wrapper.findAll('.activity-spinner')).toHaveLength(4);
  for (const change of [{ connected: false }, { gap: true }, { activity: { ...state.activity, session: 'ended' } }]) {
    await wrapper.setProps({ state: { ...state, ...change } });
    expect(wrapper.findAll('.activity-spinner')).toHaveLength(0);
  }
});

test('pending UI actions pause their animation for input and stopping preserves the active turn', async () => {
  const { publish } = application({ session, role: 'pilot', running: true, stopping: true });
  await flushPromises();
  expect(wrapper.get('#cancel').text()).toContain('Stopping');
  expect(wrapper.findAllComponents(Button).find(button => button.attributes('id') === 'cancel').props('loading')).toBe(true);
  expect(wrapper.get('#cancel').element.disabled).toBe(true);
  publish({ stopping: false, running: false, configuring: true, configOptions: options });
  await nextTick();
  expect(wrapper.get('.configuration-bar .activity-spinner').exists()).toBe(true);
  publish({ interaction: { id: 'question', epoch: 1, method: 'session/request_permission', params: { options: [] } } });
  await nextTick();
  expect(wrapper.find('.configuration-bar .activity-spinner').exists()).toBe(false);
});

test('plans show known support before observation and render native content once in source order', async () => {
  const state = { session, lines: [], planSnapshot: { version: 1, authoritative: true, support: 'supported', observation: 'not_observed', plans: {} } };
  render(TranscriptView, { state });
  expect(wrapper.get('[aria-label="Plans"]').text()).toContain('supported · not yet observed');
  expect(wrapper.get('.activity-panel').attributes()).toHaveProperty('open');
  const entries = [{ content: 'Verify', status: 'in_progress', priority: 'high' }, { content: 'Inspect', status: 'cancelled', priority: 'future' }];
  await wrapper.setProps({ state: { ...state, activity: { plans: { steps: { entries } } }, planSnapshot: { ...state.planSnapshot, observation: 'observed', plans: {
    steps: { plan: { type: 'items', planId: 'steps', entries } }, empty: { plan: { type: 'items', planId: 'empty', entries: [] } },
    other: { plan: { type: '_future', planId: 'other', payload: '<script>unsafe</script>' } },
  } } } });
  const text = wrapper.get('[aria-label="Plans"]').text();
  expect(text).toContain('supported · observed');
  expect(text.indexOf('Verify')).toBeLessThan(text.indexOf('Inspect'));
  expect(text.match(/Verify/g)).toHaveLength(1);
  expect(text).toContain('cancelled · future · Inspect');
  expect(text).toContain('Empty plan');
  expect(text).toContain('Unsupported plan content type: _future');
  expect(wrapper.find('script').exists()).toBe(false);
  await wrapper.setProps({ state: { ...state, activity: { plans: {
    stale: { type: 'items', entries: [{ content: 'Stale native plan' }] },
    'v1-plan': { source: 'acp-v1', entries: [{ content: 'Adapted v1 plan', status: 'pending', priority: 'low' }] },
  } } } });
  expect(wrapper.get('[aria-label="Plans"]').text()).not.toContain('Stale native plan');
  expect(wrapper.get('[aria-label="Plans"]').text()).toContain('Adapted v1 plan');
  expect(wrapper.get('[aria-label="Plans"]').text()).toContain('supported · not yet observed');
  await wrapper.setProps({ state: { ...state, planSnapshot: { ...state.planSnapshot, authoritative: false, observation: 'observed', plans: { steps: { plan: { type: 'items', entries } } } },
    activity: { plans: { other: { entries: [{ content: 'Retained older-daemon plan' }] } } } } });
  expect(wrapper.get('[aria-label="Plans"]').text()).toContain('Retained older-daemon plan');
  expect(wrapper.get('[aria-label="Plans"]').text()).toContain('Verify');
});

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

test('live session settings preserve distinct modes authoritative values and empty catalogs', async () => {
  const mode = { configId: 'mode', category: 'mode', name: 'Approval preset', type: 'select', currentValue: 'default', options: [{ value: 'default', name: 'Default' }, { value: 'plan', name: 'Plan' }] };
  const collaboration = { ...mode, configId: 'collaboration_mode', category: 'collaboration_mode', name: 'Collaboration mode' };
  const configOptions = [...options, mode, collaboration, { ...mode, configId: 'empty', name: 'Unavailable mode', options: [] }];
  const { client, publish } = application({ session, role: 'pilot', configOptions, lines: [{ kind: 'agent', text: 'Completed turn' }] });
  await flushPromises();
  const select = id => wrapper.findAllComponents(Select).find(item => item.props('inputId') === `config-${id}`);
  expect(wrapper.text()).toContain('Provider / Model');
  expect(wrapper.text()).toContain('Approval preset');
  expect(wrapper.text()).toContain('Collaboration mode');
  expect(select('empty').props('disabled')).toBe(true);
  expect(select('collaboration_mode').props('disabled')).toBe(false);
  client.configure.mockRejectedValueOnce(new Error('Agent rejected configuration'));
  // Exercise the component's real internal selection, not just an emitted event: it optimistically stores Plan.
  await wrapper.get('#config-collaboration_mode').trigger('click'); await flushPromises();
  document.body.querySelector('[role="option"][aria-label="Plan"]').dispatchEvent(new MouseEvent('mousedown', { bubbles: true })); await flushPromises();
  expect(client.configure).toHaveBeenCalledWith('collaboration_mode', 'plan');
  expect(wrapper.text()).toContain('Agent rejected configuration');
  expect(select('collaboration_mode').props('modelValue')).toBe('default');
  expect(wrapper.get('#config-collaboration_mode').text()).toBe('Default');
  publish({ configOptions: configOptions.map(option => option.configId === 'collaboration_mode' ? { ...option, currentValue: 'plan' } : option) }); await nextTick();
  expect(select('collaboration_mode').props('modelValue')).toBe('plan');
  expect(select('mode').props('modelValue')).toBe('default');
  publish({ role: 'observer' }); await nextTick();
  expect(select('collaboration_mode').props('disabled')).toBe(true);
  expect(select('collaboration_mode').props('modelValue')).toBe('plan');
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
  publish({ lines: [{ kind: 'tool', text: '<script>bad()</script> more' }, { kind: 'agent', text: '**Rendered**\n\n```js\nconst value = 42;\n```' }] });
  await flushPromises();
  expect(wrapper.get('.message.agent strong').text()).toBe('Rendered');
  expect(wrapper.get('.message.agent pre code').element.textContent).toBe('const value = 42;\n');
  expect(document.activeElement).toBe(input.element);
  expect(input.element.value).toBe('draft');
  expect(wrapper.get('.tool-message').element.open).toBe(true);
});

test('retained user and agent Markdown renders while notices and tools stay literal', () => {
  render(TranscriptView, { state: { session, lines: [
    { kind: 'user', text: '# Question\n\n**Details**' },
    { kind: 'agent', text: '## Answer\n\n- Explanation' },
    { kind: 'notice', text: '**Notice** <b>literal</b>' },
    { kind: 'tool', input: '`input`', text: '# Output <b>literal</b>' },
  ] } });
  expect(wrapper.get('.message.user h1').text()).toBe('Question');
  expect(wrapper.get('.message.user strong').text()).toBe('Details');
  expect(wrapper.get('.message.agent h2').text()).toBe('Answer');
  expect(wrapper.get('.message.agent li').text()).toBe('Explanation');
  expect(wrapper.get('.message.notice .message-text').text()).toBe('**Notice** <b>literal</b>');
  expect(wrapper.find('.message.notice strong, .message.notice b').exists()).toBe(false);
  expect(wrapper.findAll('.tool-message pre').map(item => item.text())).toEqual(['`input`', '# Output <b>literal</b>']);
  expect(wrapper.find('.tool-message h1, .tool-message b').exists()).toBe(false);
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
  await wrapper.setProps({ state: { session, lines: [{ kind: 'agent', text: '# First\n\n**More**' }], usage: {} } });
  await flushPromises();
  expect(wrapper.get('.message.agent h1').text()).toBe('First');
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
