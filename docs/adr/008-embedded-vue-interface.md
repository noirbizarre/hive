# 008: Embedded Vue interface

## Status

Accepted.

## Context

The browser needs a cohesive, accessible visual system while retaining the
daemon's session and transport semantics and single-process distribution.

## Decision

Adopt Vue 3.5 and OpenVue 1.0.0 with its Aura theme for the browser UI.
Use Vite to build local JavaScript and CSS before compiling Rust. Generated
assets are ignored by Git and embedded in the daemon, which remains the only
production process. Node 22.22.2 or newer is a source-build prerequisite.

The previous client used vanilla JavaScript, HTML and inline CSS; it had no Vue
version or runtime dependencies. Its npm build only checked JavaScript syntax.
Keep its tested ACP adapter and reducer independent of the Vue presentation.
The daemon remains authoritative for session identity, history, lifecycle,
reconnect and fenced exclusive control.

## Evaluation (2026-10-25)

- [OpenVue](https://openvue.dev/introduction) is the community continuation of
  PrimeVue 4.5.5, founded in July 2026. Its repository was active on October 20.
  npm has stable 1.0.0 packages, although GitHub's release listing still ends
  with an August release candidate. Two npm maintainers and a short independent
  track record warrant pinned dependencies and review before upgrades; they do
  not establish long-term sustainability.
- `openvue`, `@openvue/core` and `@openvue/themes` publish MIT licenses.
  The core peer dependency is Vue `^3.5.0`. The theme ecosystem also brings
  `@openuxkit` styled, styles, utils and themes packages. Lock the resolved tree.
  Chart.js and Quill are optional peers and are not needed here.
- Buttons, selects, text inputs, messages, tags and drawers cover Hive's needs.
  Aura's design tokens provide coherent system-following light/dark styling.
  Direct component imports keep unrelated widgets out of the production bundle.
- OpenVue documents keyboard and screen-reader behavior and claims WCAG 2.1 AA
  support. Hive must still check labels, focus, contrast, mobile navigation and
  announcements in its own composition; a library claim is not an application
  accessibility audit.
- Vue introduces a real build pipeline and a larger runtime than the previous
  client. Local assets avoid CDN availability/privacy dependencies. A bounded
  set of fixed output filenames keeps Rust embedding simple, with revalidation
  to prevent stale cached bundles. Measure production raw/gzip sizes during
  verification rather than using npm's unpacked package size as a bundle metric.

The available packages meet the compatibility and component requirements, so
there is no concrete blocker requiring a competing library evaluation. Keep
protocol logic separate to reduce the cost of replacing the presentation later.

## Consequences

The production build measures approximately 472 kB JavaScript (114 kB gzip) and
7.5 kB CSS (2.3 kB gzip), plus the HTML shell and canonical icon. Theme/component
styles are also injected from the JavaScript bundle. These are Vite 7.3.7's
reported decimal sizes for the initial implementation, not a size guarantee.

### Build contract

Run `npm ci --prefix clients/hive-web` and
`npm run build --prefix clients/hive-web` before Cargo. CI does the same on
every Rust-compiling platform. Cargo does not install npm dependencies or fetch
frontend packages. The binary serves the resulting HTML, JavaScript, CSS and
canonical Hive icon under `/ui`; no separate frontend server is required.

See the [browser guide](../guides/web.md) for workflow and accessibility checks.
