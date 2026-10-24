# Changelog

All notable changes to this project will be documented in this file.

This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.1.0 - 2026-10-24

### 💫 Features

- **activity** Stream live agent execution state ([#22](https://github.com/noirbizarre/hive/issues/22)) - ([4e92f3d](https://github.com/noirbizarre/hive/commit/4e92f3d66b0ed9434a6cc729b1c492c0fa0405c2))
- **cli** Add a hive doctor command to diagnose configuration and agent compatibility ([#29](https://github.com/noirbizarre/hive/issues/29)) - ([506b91a](https://github.com/noirbizarre/hive/commit/506b91a7593cb5eb0fb3b9887c6f2515cb961cd0))
- **cli** Colorize help output with clap styles - ([c9c5c65](https://github.com/noirbizarre/hive/commit/c9c5c651f44a11d8e02230a396d75419e4971019))
- **cli** Unify Hive commands in dedicated executable - ([4787ed0](https://github.com/noirbizarre/hive/commit/4787ed09ae9dd50be47ceade1438687b73e40096))
- **compat** Establish and document Pi ACP entrypoint compatibility ([#27](https://github.com/noirbizarre/hive/issues/27)) - ([2f09cb8](https://github.com/noirbizarre/hive/commit/2f09cb83ffd63f1d8277848ad0ec80015fdbce7a))
- **daemon** Persist and recover Hive sessions ([#23](https://github.com/noirbizarre/hive/issues/23)) - ([2674af3](https://github.com/noirbizarre/hive/commit/2674af3ef242c46018e8f459b3414784f7e8ab56))
- **daemon** Expose shared session summaries and activity - ([2e688eb](https://github.com/noirbizarre/hive/commit/2e688eb5dad36cd8324e6ad23862a439b3cfb6c2))
- **interactions** Forward ACP permissions and elicitation across clients ([#35](https://github.com/noirbizarre/hive/issues/35)) - ([1807b30](https://github.com/noirbizarre/hive/commit/1807b30bcb08a8a723da444e8b02df6b52d8cd44))
- **opencode** Adopt persistent sessions from an OpenCode server ([#28](https://github.com/noirbizarre/hive/issues/28)) - ([f962598](https://github.com/noirbizarre/hive/commit/f9625988eef11ec189e2230de2d64757fb1bbe63))
- **tui** Adopt OpenCode-inspired system theme layout - ([5daf808](https://github.com/noirbizarre/hive/commit/5daf808645e9cfa0565e5ce6eaeb695105c08984))
- **ui** Add shared multi-session navigation - ([184d888](https://github.com/noirbizarre/hive/commit/184d888242b238cf65c9050707cbfd1c845d4a4c))
- **usage** Track ACP tokens and estimate model costs ([#25](https://github.com/noirbizarre/hive/issues/25)) - ([d632218](https://github.com/noirbizarre/hive/commit/d6322183599a5540a8f2f11aad06f1eddca798b8))
- **web** Resume shared sessions with explicit handoff - ([2ed41fa](https://github.com/noirbizarre/hive/commit/2ed41fae1fedf7f576b0231416587a1e6cb5b092))
- **zellij** Attach to existing Hive sessions ([#24](https://github.com/noirbizarre/hive/issues/24)) - ([ef68dee](https://github.com/noirbizarre/hive/commit/ef68dee18f0d2970cb040e4ec106a7e689247b60))
- Build resumable pilot-and-observer TUI - ([e32b309](https://github.com/noirbizarre/hive/commit/e32b309a71e4b223b6b8988c5e379d27b90b15e7))
- Transfer shared session control across clients - ([4601143](https://github.com/noirbizarre/hive/commit/46011434273bf7b7e81eb1c3b73fa97409de66f1))
- Manage multiple agents from one Hive daemon - ([13cb88f](https://github.com/noirbizarre/hive/commit/13cb88f13080971fb3c5da6b72e4a1bbcc57de48))
- Bootstrap ACPv2 gateway workspace - ([a21ed9d](https://github.com/noirbizarre/hive/commit/a21ed9d6ec5e993080052eae0a7e2c6cf6caf16e))

### 🐛 Bug Fixes

- **tui** Show actionable empty states for agents and sessions ([#38](https://github.com/noirbizarre/hive/issues/38)) - ([93ccd98](https://github.com/noirbizarre/hive/commit/93ccd98539fb0291c8d905455305d5225914b2bb))
- Report Windows daemon lock contention clearly - ([8b552b9](https://github.com/noirbizarre/hive/commit/8b552b9ca6f21062d664fafa609d96d91ce2273f))
- Lock mise download URLs for CI platforms - ([a799362](https://github.com/noirbizarre/hive/commit/a7993628d9caf237a8d92933dceec1534940819d))

### 🔨 Refactor

- **http**  🚨 **breaking** Serve the web UI at /ui and remove the /hive prefix ([#39](https://github.com/noirbizarre/hive/issues/39)) - ([9cf7f30](https://github.com/noirbizarre/hive/commit/9cf7f30de76d06d2f0f64f3d3851228010c6bf76))
- **zellij**  🚨 **breaking** Extract ZJHive into its own repository ([#40](https://github.com/noirbizarre/hive/issues/40)) - ([0f0662b](https://github.com/noirbizarre/hive/commit/0f0662b0fe77faedba2794759c65608d332f22ec))

### 📚 Documentation

- Overhaul user guides and architecture documentation ([#36](https://github.com/noirbizarre/hive/issues/36)) - ([0100e38](https://github.com/noirbizarre/hive/commit/0100e387d91f886b8af7eab14ca72f866401decc))
- Clarify Pi ACP entrypoint compatibility ([#20](https://github.com/noirbizarre/hive/issues/20)) - ([46854d6](https://github.com/noirbizarre/hive/commit/46854d6ef69b3ce3bf459744d50d2e795fc61545))
- Align roadmap with gh-ship release gate ([#21](https://github.com/noirbizarre/hive/issues/21)) - ([307bdd8](https://github.com/noirbizarre/hive/commit/307bdd8f2247b199563c70449f3b9d5dde609236))
- Add hive branding to README and documentation ([#12](https://github.com/noirbizarre/hive/issues/12)) - ([cd39b9f](https://github.com/noirbizarre/hive/commit/cd39b9f2ac67fa7ee29b6bd38d1eb8f55390c3cf))
- Link roadmap from documentation index - ([7c30a9a](https://github.com/noirbizarre/hive/commit/7c30a9a544eb6742abec929da38d473c32d99a1b))
- Add session handoff roadmap - ([032dd3d](https://github.com/noirbizarre/hive/commit/032dd3d9cfc0b57211b8c7730880b6eacdd66647))

### 🧪 Tests

- **usage** Stop the mock agent racing the first prompt with an idle ([#30](https://github.com/noirbizarre/hive/issues/30)) - ([0309ca3](https://github.com/noirbizarre/hive/commit/0309ca3338b613a8517eaaf261119688cc6c5f81))
- Add opt-in real-agent ACP compatibility probes - ([d1a3818](https://github.com/noirbizarre/hive/commit/d1a3818197d703c7e6e163745e1b9a3b0b22248f))

### 🏗️ Build

- **tasks** Namespace `hive:tui` - ([5dece0e](https://github.com/noirbizarre/hive/commit/5dece0ec68149a7713e34c155f15aca822a3226f))

### 🧹 Chores

- Adopt rust workspace template - ([05def60](https://github.com/noirbizarre/hive/commit/05def60c1d7cfe28dd25755cc5d6d9200d4774b2))

### Tpl

- Render rust at main - ([2ec9a81](https://github.com/noirbizarre/hive/commit/2ec9a81da1a9ab356504cbb96dab0e09ce867dbe))

## ❤️ New Contributors

* @noirbizarre made their first contribution in [#40](https://github.com/noirbizarre/hive/pull/40)
