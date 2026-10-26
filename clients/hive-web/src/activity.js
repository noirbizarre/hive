// Unknown extension statuses must remain visible without implying execution.
export const executing = status => status === 'in_progress' || status === 'running';
export const live = state => state.connected && !state.gap && state.activity?.session !== 'ended';
export const working = state => live(state) && !state.interaction && state.activity?.foreground === 'running';
