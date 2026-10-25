# ADR-009: Agent-owned history and the Hive session registry

## Status

Accepted. Extends ADR-001 and ADR-003's volatile replay and durable catalog
decisions; clarifies ADR-002's bounded native backfill. Implements issue #44.

## Context

An agent can already own durable conversations. A mandatory Hive mirror would
duplicate sensitive content and create competing transcript authorities. The
existing SQLite store is not that mirror: it holds the session registry and a
prompt safety journal. Its original full submitted content and response payloads
are unnecessary for duplicate-execution protection; a fingerprint and minimal
outcome suffice. Removing reservations entirely would lose cross-restart safety.

ACP replay promises retained history, not unlimited retention. The protocol has
no standard certificate of original-history completeness. Negotiation, successful
replay, nonempty messages, and usage totals cannot provide such a certificate.

## Decision

- Agents own retained transcripts. Hive owns session identity, workspace and
  orchestration metadata, prompt safety, and a volatile display projection.
- Keep only request keys, domain-separated versioned SHA-256 content fingerprints,
  acknowledged message IDs and failure markers in the request journal. Do not
  persist prompt bodies, arbitrary acknowledgement metadata or downstream error
  text. Use the same canonical content fingerprint live, after restart and during
  migration; object-key ordering does not change identity, but arrays, exact
  strings, content metadata and serialized numeric distinctions do.
- Select restoration by protocol: advertised v1 load before context-only resume;
  v2's advertised session surface provides baseline resume with `start` replay.
  No implementation-name exceptions or invented replay capabilities.
- Expose ownership, projection source, restoration support, availability,
  completeness and controlled warnings through one additive `transcript` object.
  These are independent of session lifecycle and recovery acknowledgement.
- Render available history with warnings. Successful empty replay remains usable;
  earlier accounting without returned messages produces a warning, not proof of
  protocol violation. Known local truncation is partial; otherwise original
  retention coverage remains unknown.
- Warn when advertised operations are unimplemented or required resume state is
  missing. Unsupported replay and actual restoration failures remain actionable.
  Do not create replacement sessions or resend uncertain prompts.
- Retain existing snapshot messages and atomic snapshot/live boundaries. Clients
  stage content and status until attachment succeeds and reject stale status.
- Do not add an archive now. A future opt-in archive needs an explicit purpose
  (such as offline access, search or audit), authority/fallback rules, retention,
  deletion, access and migration policies before storing transcript payloads.

## Consequences

Registry listing works without conversation payloads or an available agent.
Daemon restart loses the volatile view and depends on the agent's retained
history. Without replay, new live content can still form a partial view.
Unavailable agents leave identifiable offline records, not an offline archive.

Schema version 2 transactionally fingerprints existing prompt bodies and blanks
the obsolete body column, reduces successful results to message IDs and errors
to failure markers, and preserves unfinished reservations and registry JSON.
Invalid rows roll back the upgrade, never release a key. Older binaries refuse
the new schema. Durable retries retain the original message ID but cannot return
the original arbitrary response metadata or failure text.

Existing close deletion and 90-day orphaned/lost pruning rules are unchanged.
Registry titles still contain user-derived text; fingerprints are not encryption.
Logical removal does not promise forensic erasure of old pages/WAL or backups;
there is no automatic compaction or backup deletion.

This rejects mandatory transcript mirroring and implementation-specific recovery
heuristics. It also rejects labeling successful replay as complete or blocking
every partial view, because neither policy follows the protocol's guarantees.
