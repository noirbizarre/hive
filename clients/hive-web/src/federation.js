// Logical identity must survive gateway aliases changing after reconnect.
export const sessionKey = session => session?.reference ? JSON.stringify([session.reference.owner, session.reference.id]) : session?.sessionId;
export const ownerPathIsAbsolute = path => {
  if (typeof path !== 'string') return false;
  const unc = path.startsWith('\\\\') && path.slice(2).split(/[\\/]/);
  return path.startsWith('/') || /^[A-Za-z]:[\\/]/.test(path) || Boolean(unc && unc[0] && unc[1]);
};
const validOwner = owner => /^node-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(owner || '');

export function negotiate(response) {
  const hive = response.capabilities?._meta?.hive;
  if (response.protocolVersion !== 2 || !response.capabilities?.session || hive?.version !== 1 || !validOwner(hive.node)) {
    throw new Error('Hive ACP discovery unavailable — connect to a persistent Hive supporting extension version 1');
  }
  const features = hive.features;
  if (!features || typeof features !== 'object' || Array.isArray(features)) throw new Error('Malformed Hive feature advertisement');
  for (const name of ['agents', 'selection', 'control', 'location', 'peers', 'workspaces', 'profiles']) {
    const value = features[name];
    if (value != null && (typeof value !== 'object' || Array.isArray(value) || Object.keys(value).length)) throw new Error('Malformed Hive feature advertisement');
  }
  const enabled = name => features[name] != null;
  for (const [name, required] of [['selection', ['agents', 'control']], ['location', ['control']], ['peers', ['location', 'selection']], ['profiles', ['selection']], ['workspaces', ['selection']]]) {
    if (enabled(name) && required.some(feature => !enabled(feature))) throw new Error('Inconsistent Hive feature advertisement');
  }
  return { node: hive.node, enabled };
}

export function discovery(request, negotiated) {
  const labels = new Map([[negotiated.node, 'Local']]);
  async function pages(method, field, owner) {
    const entries = [], cursors = new Set();
    let cursor, defaults;
    do {
      const page = await request(method, { ...(owner ? { peer: owner } : {}), ...(cursor ? { cursor } : {}) });
      entries.push(...page[field]);
      defaults ??= page;
      cursor = page.nextCursor;
      // A malformed continuation must not hold a client's refresh loop forever.
      if (cursor && cursors.has(cursor)) throw new Error('Hive inventory repeated a continuation cursor; refresh discovery');
      cursors.add(cursor);
    } while (cursor);
    return { ...defaults, [field]: entries };
  }
  return {
    async owners() {
      const peers = negotiated.enabled('peers') ? (await request('_hive/peers/list', {})).peers : [];
      for (const peer of peers) labels.set(peer.owner, peer.name || peer.alias || peer.owner);
      return [{ owner: negotiated.node, name: 'Local', local: true }, ...peers];
    },
    async inventory(owner) {
      const target = owner && owner !== negotiated.node ? owner : undefined;
      if (target && !negotiated.enabled('peers')) throw new Error('Remote discovery unsupported by this Hive');
      if (!negotiated.enabled('agents')) throw new Error('Agent inventory unsupported by this Hive');
      const agents = await pages('_hive/agents/list', 'agents', target);
      const profiles = negotiated.enabled('profiles') ? await pages('_hive/profiles/list', 'profiles', target) : { profiles: [] };
      return { ...agents, ...profiles };
    },
    async sessions() {
      const rows = [], cursors = new Set();
      let cursor, metadata;
      do {
        const page = await request('session/list', cursor ? { cursor } : {});
        for (const entry of page.sessions || []) {
          const hive = entry._meta?.hive || {};
          // Owner summaries can contain their native sessionId; only the outer ID is routable here.
          rows.push({ ...hive, ...entry, reference: hive.session, availability: hive.availability, ownerName: labels.get(hive.session?.owner) });
        }
        metadata = page._meta?.hive;
        cursor = page.nextCursor;
        if (cursor && cursors.has(cursor)) throw new Error('Hive session list repeated a continuation cursor');
        cursors.add(cursor);
      } while (cursor);
      return { sessions: rows, ...metadata };
    },
    async resolve(session) {
      if (!session.reference) return session;
      if (!validOwner(session.reference.owner) || typeof session.reference.id !== 'string' || !session.reference.id) throw new Error('Invalid owner-qualified session reference');
      if (!negotiated.enabled('location')) throw new Error('Session resolution unsupported by this Hive');
      const location = await request('_hive/sessions/resolve', { session: session.reference });
      if (JSON.stringify([location.session?.owner, location.session?.id]) !== sessionKey(session) || !location.sessionId || !ownerPathIsAbsolute(location.cwd)) throw new Error('Hive returned a stale session route; resolve again');
      return { ...session, sessionId: location.sessionId, cwd: location.cwd };
    },
  };
}
