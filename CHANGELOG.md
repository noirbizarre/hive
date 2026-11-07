# Changelog

All notable changes to this project will be documented in this file.

This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.1.0 - 2026-11-07

### 💫 Features

- **activity** Stream live agent execution state ([#22](https://github.com/noirbizarre/hive/issues/22)) - ([4e92f3d](https://github.com/noirbizarre/hive/commit/4e92f3d66b0ed9434a6cc729b1c492c0fa0405c2))
- **cli** Add a hive doctor command to diagnose configuration and agent compatibility ([#29](https://github.com/noirbizarre/hive/issues/29)) - ([506b91a](https://github.com/noirbizarre/hive/commit/506b91a7593cb5eb0fb3b9887c6f2515cb961cd0))
- **cli** Colorize help output with clap styles - ([c9c5c65](https://github.com/noirbizarre/hive/commit/c9c5c651f44a11d8e02230a396d75419e4971019))
- **cli** Unify Hive commands in dedicated executable - ([4787ed0](https://github.com/noirbizarre/hive/commit/4787ed09ae9dd50be47ceade1438687b73e40096))
- **compat** Establish and document Pi ACP entrypoint compatibility ([#27](https://github.com/noirbizarre/hive/issues/27)) - ([2f09cb8](https://github.com/noirbizarre/hive/commit/2f09cb83ffd63f1d8277848ad0ec80015fdbce7a))
- **config** Support explicit configuration fragments ([#116](https://github.com/noirbizarre/hive/issues/116)) - ([903c2fa](https://github.com/noirbizarre/hive/commit/903c2fa0cc66a06afee3d9df9d7bfec958f7204a))
- **config** Reload daemon configuration without restarting ([#49](https://github.com/noirbizarre/hive/issues/49)) - ([cf2e024](https://github.com/noirbizarre/hive/commit/cf2e0241ee6af4869be3274251325201b1f298b2))
- **core** Define project and workspace domain contracts ([#119](https://github.com/noirbizarre/hive/issues/119)) - ([84c2b55](https://github.com/noirbizarre/hive/commit/84c2b55fb182a468bdbb8e8ff93169b5773e9bcc))
- **daemon** Persist and recover Hive sessions ([#23](https://github.com/noirbizarre/hive/issues/23)) - ([2674af3](https://github.com/noirbizarre/hive/commit/2674af3ef242c46018e8f459b3414784f7e8ab56))
- **daemon** Expose shared session summaries and activity - ([2e688eb](https://github.com/noirbizarre/hive/commit/2e688eb5dad36cd8324e6ad23862a439b3cfb6c2))
- **federation** Integrate remote sessions into TUI and web UI ([#167](https://github.com/noirbizarre/hive/issues/167)) - ([f37a816](https://github.com/noirbizarre/hive/commit/f37a816cbd438bd8f1998ed56aa0165de2a02946))
- **interactions** Forward ACP permissions and elicitation across clients ([#35](https://github.com/noirbizarre/hive/issues/35)) - ([1807b30](https://github.com/noirbizarre/hive/commit/1807b30bcb08a8a723da444e8b02df6b52d8cd44))
- **mcp** Align server declarations with Bee configuration ([#166](https://github.com/noirbizarre/hive/issues/166)) - ([23899d3](https://github.com/noirbizarre/hive/commit/23899d39388946c4e658c50225cb275683fa852e))
- **mcp** Route managed servers through an aggregated ACP service ([#155](https://github.com/noirbizarre/hive/issues/155)) - ([b6063df](https://github.com/noirbizarre/hive/commit/b6063dff49921f58305bc30f72debf27a3cc872e))
- **mcp** Resolve scoped instances from registered resources ([#152](https://github.com/noirbizarre/hive/issues/152)) - ([995e604](https://github.com/noirbizarre/hive/commit/995e604c4fa01a63deb682cb9116f08ac13dc770))
- **mcp** Add shared stdio runtime and request multiplexing ([#148](https://github.com/noirbizarre/hive/issues/148)) - ([6e36048](https://github.com/noirbizarre/hive/commit/6e360482e64555fb89346393f4717c120626ad5b))
- **mcp** Define transport, scope and instance identity ([#146](https://github.com/noirbizarre/hive/issues/146)) - ([d34207c](https://github.com/noirbizarre/hive/commit/d34207c1caf90c94820aea642656c85b8eaa24e0))
- **opencode** Adopt persistent sessions from an OpenCode server ([#28](https://github.com/noirbizarre/hive/issues/28)) - ([f962598](https://github.com/noirbizarre/hive/commit/f9625988eef11ec189e2230de2d64757fb1bbe63))
- **plans** Support ACP v2 plans end-to-end ([#59](https://github.com/noirbizarre/hive/issues/59)) - ([1d5648d](https://github.com/noirbizarre/hive/commit/1d5648d8d4ea8522ea06b52b607f82af10899f69))
- **profiles** Add named execution profiles ([#157](https://github.com/noirbizarre/hive/issues/157)) - ([124f7da](https://github.com/noirbizarre/hive/commit/124f7dab6df080688c84b9c75c5e458175246d06))
- **project** Add a logical project registry ([#120](https://github.com/noirbizarre/hive/issues/120)) - ([4c46496](https://github.com/noirbizarre/hive/commit/4c46496435fc106ff819ad1651b80ee0bdad604a))
- **remote** Route and resume owner-managed ACP sessions ([#161](https://github.com/noirbizarre/hive/issues/161)) - ([836065c](https://github.com/noirbizarre/hive/commit/836065c4b8369319678387e355370d00ee19cb6c))
- **remote** Expose agent discovery and startup over ACP ([#150](https://github.com/noirbizarre/hive/issues/150)) - ([879a304](https://github.com/noirbizarre/hive/commit/879a304db06b084c1c8c1790b4e31e70378c546d))
- **sessions** Expose native agent modes and live settings ([#72](https://github.com/noirbizarre/hive/issues/72)) - ([a3bd217](https://github.com/noirbizarre/hive/commit/a3bd217d4dfd1b1d32281ca73300aefd2c8584a9))
- **sessions** Fix session attachment and add provider/model selection ([#41](https://github.com/noirbizarre/hive/issues/41)) - ([9a8b711](https://github.com/noirbizarre/hive/commit/9a8b7110d4a9413b940ed549b582406f727a4931))
- **transport** Add validated Streamable HTTP peer connectivity ([#149](https://github.com/noirbizarre/hive/issues/149)) - ([01fe417](https://github.com/noirbizarre/hive/commit/01fe417e2a2652255922f06386e8c5afc4f61e42))
- **tui** Report agent status through OSC 7501 ([#156](https://github.com/noirbizarre/hive/issues/156)) - ([cd5acb7](https://github.com/noirbizarre/hive/commit/cd5acb779d4c8dffd8084c9e0d45dda0465b8808))
- **tui** Support OSC 5522 image clipboard ([#154](https://github.com/noirbizarre/hive/issues/154)) - ([f7c3f47](https://github.com/noirbizarre/hive/commit/f7c3f47bf5eb84ad540fe7b10a853fbf6765dcae))
- **tui** Support OSC 52 clipboard writes ([#151](https://github.com/noirbizarre/hive/issues/151)) - ([8183ec5](https://github.com/noirbizarre/hive/commit/8183ec5b2280675eefe168642abea0e3232de7b2))
- **tui** Render ACP images in conversation messages ([#145](https://github.com/noirbizarre/hive/issues/145)) - ([ff35092](https://github.com/noirbizarre/hive/commit/ff3509207dadc7adb58e123d071e4a781347e19e))
- **tui** Add Kitty image rendering backend ([#134](https://github.com/noirbizarre/hive/issues/134)) - ([db1a181](https://github.com/noirbizarre/hive/commit/db1a1816999f9afde42f37fbd2e15b635c0085fa))
- **tui** Detect and configure terminal capabilities ([#133](https://github.com/noirbizarre/hive/issues/133)) - ([509be9c](https://github.com/noirbizarre/hive/commit/509be9ce88777df34c7c8126430e5e4b3ea7e282))
- **tui** Add prompt editing, clipboard, and external editor ([#69](https://github.com/noirbizarre/hive/issues/69)) - ([cfa6974](https://github.com/noirbizarre/hive/commit/cfa6974e8e9cdb4356223c789753ac169b1b5901))
- **tui** Render markdown and expandable tools with history scrolling - ([eafdf5f](https://github.com/noirbizarre/hive/commit/eafdf5f3b4e2765458cafdd6d9722a51a3f954d7))
- **tui** Adopt OpenCode-inspired system theme layout - ([5daf808](https://github.com/noirbizarre/hive/commit/5daf808645e9cfa0565e5ce6eaeb695105c08984))
- **ui** Add in-progress spinners to web and terminal clients ([#75](https://github.com/noirbizarre/hive/issues/75)) - ([f9e6a41](https://github.com/noirbizarre/hive/commit/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17))
- **ui** Add shared multi-session navigation - ([184d888](https://github.com/noirbizarre/hive/commit/184d888242b238cf65c9050707cbfd1c845d4a4c))
- **usage** Track ACP tokens and estimate model costs ([#25](https://github.com/noirbizarre/hive/issues/25)) - ([d632218](https://github.com/noirbizarre/hive/commit/d6322183599a5540a8f2f11aad06f1eddca798b8))
- **web** Refresh embedded UI with Vue and OpenVue ([#54](https://github.com/noirbizarre/hive/issues/54)) - ([6423735](https://github.com/noirbizarre/hive/commit/642373549df2734fecd80450ffc494c5615cc8f2))
- **web** Resume shared sessions with explicit handoff - ([2ed41fa](https://github.com/noirbizarre/hive/commit/2ed41fae1fedf7f576b0231416587a1e6cb5b092))
- **workspace** Expose safe lifecycle tools to agents ([#171](https://github.com/noirbizarre/hive/issues/171)) - ([dbe2273](https://github.com/noirbizarre/hive/commit/dbe2273d7a87aa3976d71c047cb8f2e2d9514027))
- **workspace** Expose backend-neutral VCS tools to agents ([#163](https://github.com/noirbizarre/hive/issues/163)) - ([3ab2be1](https://github.com/noirbizarre/hive/commit/3ab2be11480c944119e51dca8dfe25d7d8e631c7))
- **workspace** Expose session-scoped capability tools ([#159](https://github.com/noirbizarre/hive/issues/159)) - ([3538ab8](https://github.com/noirbizarre/hive/commit/3538ab82f6d6535f5c8f3f9dca2cbc8b8f894f17))
- **workspace** Provision projects on the selected federated node ([#153](https://github.com/noirbizarre/hive/issues/153)) - ([d0c29c0](https://github.com/noirbizarre/hive/commit/d0c29c06c98ffcc49cafba9b33e2db35b20dc190))
- **workspace** Add native Jujutsu support ([#139](https://github.com/noirbizarre/hive/issues/139)) - ([91e720a](https://github.com/noirbizarre/hive/commit/91e720abe9ba398f0f7d2cced095e367c8e2d681))
- **workspace** Recover interrupted operations and add safe cleanup sweeps ([#135](https://github.com/noirbizarre/hive/issues/135)) - ([2812df3](https://github.com/noirbizarre/hive/commit/2812df37518aa3791259819a742836c4f913facf))
- **workspace** Complete pre-session provisioning contract ([#132](https://github.com/noirbizarre/hive/issues/132)) - ([7036b81](https://github.com/noirbizarre/hive/commit/7036b81f045f99ba57f785fedfa10591c3d55be6))
- **workspace** Provision cached repositories and task worktrees ([#131](https://github.com/noirbizarre/hive/issues/131)) - ([530e994](https://github.com/noirbizarre/hive/commit/530e99483aaf50049e8def0ebd6fc8aeb06e3ad1))
- **worktrunk** Publish aggregated Hive agent activity ([#140](https://github.com/noirbizarre/hive/issues/140)) - ([ecd04a5](https://github.com/noirbizarre/hive/commit/ecd04a580da229b253e4212a8a8e8eac11aad814))
- **zellij** Attach to existing Hive sessions ([#24](https://github.com/noirbizarre/hive/issues/24)) - ([ef68dee](https://github.com/noirbizarre/hive/commit/ef68dee18f0d2970cb040e4ec106a7e689247b60))
- Build resumable pilot-and-observer TUI - ([e32b309](https://github.com/noirbizarre/hive/commit/e32b309a71e4b223b6b8988c5e379d27b90b15e7))
- Transfer shared session control across clients - ([4601143](https://github.com/noirbizarre/hive/commit/46011434273bf7b7e81eb1c3b73fa97409de66f1))
- Manage multiple agents from one Hive daemon - ([13cb88f](https://github.com/noirbizarre/hive/commit/13cb88f13080971fb3c5da6b72e4a1bbcc57de48))
- Bootstrap ACPv2 gateway workspace - ([a21ed9d](https://github.com/noirbizarre/hive/commit/a21ed9d6ec5e993080052eae0a7e2c6cf6caf16e))

### 🐛 Bug Fixes

- **acp** Ignore idle snapshots before foreground work starts ([#102](https://github.com/noirbizarre/hive/issues/102)) - ([c44a5f3](https://github.com/noirbizarre/hive/commit/c44a5f340341e12720924b752a2ea78bb7ab8b84))
- **acp** Reconcile activity chunks on content replacement ([#100](https://github.com/noirbizarre/hive/issues/100)) - ([480edfe](https://github.com/noirbizarre/hive/commit/480edfef73ac5e26524c3de9a5638b6d14963bbd))
- **acp** Cancel active work before closing sessions ([#99](https://github.com/noirbizarre/hive/issues/99)) - ([711854c](https://github.com/noirbizarre/hive/commit/711854c2fb93b0efc89d3f8def93f8935cd1bb12))
- **acp** Treat requires_action as active foreground work ([#98](https://github.com/noirbizarre/hive/issues/98)) - ([2ed5a07](https://github.com/noirbizarre/hive/commit/2ed5a07e16af9399be4dce6620b4824a7969b482))
- **acp** Emit current state before resume responses ([#97](https://github.com/noirbizarre/hive/issues/97)) - ([a63da5e](https://github.com/noirbizarre/hive/commit/a63da5ee1517547d42372caa3557a127107c0da7))
- **acp** Preserve omitted versus null transcript patches ([#91](https://github.com/noirbizarre/hive/issues/91)) - ([32201e0](https://github.com/noirbizarre/hive/commit/32201e0a4cafd3df12a5df338387d09fc6bfa1fb))
- **acp** Update SDK baseline and connection cleanup ([#90](https://github.com/noirbizarre/hive/issues/90)) - ([7e01c5d](https://github.com/noirbizarre/hive/commit/7e01c5d92aa415828e32ce67468f1c4997cab766))
- **acp** Preserve stop reasons and surface native failures ([#74](https://github.com/noirbizarre/hive/issues/74)) - ([090233d](https://github.com/noirbizarre/hive/commit/090233d4a06abe8f9217b843cddec46e012cc18c))
- **ci** Bootstrap mise before relocating Windows temporary storage - ([73873ac](https://github.com/noirbizarre/hive/commit/73873acdc57b2354a3c546789b044025889a5264))
- **ci** Pin Bee to stable 0.2.0 release ([#106](https://github.com/noirbizarre/hive/issues/106)) - ([57d5c75](https://github.com/noirbizarre/hive/commit/57d5c75db1b7b7fe8d0d9a3e046659db0fd72758))
- **ci** Refresh rolling Bee asset checksums - ([636f849](https://github.com/noirbizarre/hive/commit/636f8491b55b90ed1e0efe149109f0de6d6cb4a9))
- **opencode** Require client approval before leaving Plan mode ([#103](https://github.com/noirbizarre/hive/issues/103)) - ([6cc84e6](https://github.com/noirbizarre/hive/commit/6cc84e6539ff43b1b270d1a259a0b72d20e70633))
- **sessions** Harden recovery after uncertain prompt delivery ([#101](https://github.com/noirbizarre/hive/issues/101)) - ([07e3d53](https://github.com/noirbizarre/hive/commit/07e3d5389424074f9b3530c484d710740cea5203))
- **sessions** Restore existing ACP conversations ([#50](https://github.com/noirbizarre/hive/issues/50)) - ([abd263f](https://github.com/noirbizarre/hive/commit/abd263fbc4576419b09537ca7820ee6b5ad63881))
- **tests** Remove wall-clock waits from daemon tests ([#169](https://github.com/noirbizarre/hive/issues/169)) - ([1317f5a](https://github.com/noirbizarre/hive/commit/1317f5a1f98c09812a4537ffc6f9b5d498811770))
- **tests** Isolate mock-agent coverage profiles ([#168](https://github.com/noirbizarre/hive/issues/168)) - ([4559363](https://github.com/noirbizarre/hive/commit/4559363e470be9f2b0063ee971b7c9b3691ee66d))
- **tests** Synchronize subprocess cleanup and split slow scenarios - ([9475249](https://github.com/noirbizarre/hive/commit/9475249a9bf2a21ef82905addb9158f87717c8d6))
- **tui** Distinguish empty model catalogs from unsupported selection ([#48](https://github.com/noirbizarre/hive/issues/48)) - ([43afa11](https://github.com/noirbizarre/hive/commit/43afa11e4f972e7cf37ba659688f6fec32cc422c))
- **tui** Add fuzzy model modal and exit cleanly - ([dd0b755](https://github.com/noirbizarre/hive/commit/dd0b755da46090aa6f1a1a7b709c999bdd640660))
- **tui** Show actionable empty states for agents and sessions ([#38](https://github.com/noirbizarre/hive/issues/38)) - ([93ccd98](https://github.com/noirbizarre/hive/commit/93ccd98539fb0291c8d905455305d5225914b2bb))
- **web** Render Markdown in discussions ([#68](https://github.com/noirbizarre/hive/issues/68)) - ([e9432fe](https://github.com/noirbizarre/hive/commit/e9432fe048dc39ed48adb8d2e608c09ba9d17a87))
- Report Windows daemon lock contention clearly - ([8b552b9](https://github.com/noirbizarre/hive/commit/8b552b9ca6f21062d664fafa609d96d91ce2273f))
- Lock mise download URLs for CI platforms - ([a799362](https://github.com/noirbizarre/hive/commit/a7993628d9caf237a8d92933dceec1534940819d))

### 🔨 Refactor

- **http**  🚨 **breaking** Serve the web UI at /ui and remove the /hive prefix ([#39](https://github.com/noirbizarre/hive/issues/39)) - ([9cf7f30](https://github.com/noirbizarre/hive/commit/9cf7f30de76d06d2f0f64f3d3851228010c6bf76))
- **sessions** Separate registry from agent-owned history ([#55](https://github.com/noirbizarre/hive/issues/55)) - ([38e98ad](https://github.com/noirbizarre/hive/commit/38e98ad8938efc3c98ff7760ec7cfcec4fc2229e))
- **workspace** Separate repository backends from provisioning ([#138](https://github.com/noirbizarre/hive/issues/138)) - ([983aae0](https://github.com/noirbizarre/hive/commit/983aae007cc4f268755833afe65371db1c39f0a0))
- **zellij**  🚨 **breaking** Extract ZJHive into its own repository ([#40](https://github.com/noirbizarre/hive/issues/40)) - ([0f0662b](https://github.com/noirbizarre/hive/commit/0f0662b0fe77faedba2794759c65608d332f22ec))

### 📚 Documentation

- **acp** Specify remote discovery and session routing extensions ([#147](https://github.com/noirbizarre/hive/issues/147)) - ([c632c98](https://github.com/noirbizarre/hive/commit/c632c98b774bf918a29c626677460826e2066be3))
- **acp** Document complete protocol compatibility matrix ([#73](https://github.com/noirbizarre/hive/issues/73)) - ([044c355](https://github.com/noirbizarre/hive/commit/044c3559e3cef00c428379193e4777620ec72d43))
- Overhaul user guides and architecture documentation ([#36](https://github.com/noirbizarre/hive/issues/36)) - ([0100e38](https://github.com/noirbizarre/hive/commit/0100e387d91f886b8af7eab14ca72f866401decc))
- Clarify Pi ACP entrypoint compatibility ([#20](https://github.com/noirbizarre/hive/issues/20)) - ([46854d6](https://github.com/noirbizarre/hive/commit/46854d6ef69b3ce3bf459744d50d2e795fc61545))
- Align roadmap with gh-ship release gate ([#21](https://github.com/noirbizarre/hive/issues/21)) - ([307bdd8](https://github.com/noirbizarre/hive/commit/307bdd8f2247b199563c70449f3b9d5dde609236))
- Add hive branding to README and documentation ([#12](https://github.com/noirbizarre/hive/issues/12)) - ([cd39b9f](https://github.com/noirbizarre/hive/commit/cd39b9f2ac67fa7ee29b6bd38d1eb8f55390c3cf))
- Link roadmap from documentation index - ([7c30a9a](https://github.com/noirbizarre/hive/commit/7c30a9a544eb6742abec929da38d473c32d99a1b))
- Add session handoff roadmap - ([032dd3d](https://github.com/noirbizarre/hive/commit/032dd3d9cfc0b57211b8c7730880b6eacdd66647))

### 🧪 Tests

- **acp** Add deterministic Bee integration coverage ([#104](https://github.com/noirbizarre/hive/issues/104)) - ([dbb21a6](https://github.com/noirbizarre/hive/commit/dbb21a603db5fb5668a5054284b9341b0caa95e7))
- **sessions** Cover transcript replay and reconnect across agents ([#108](https://github.com/noirbizarre/hive/issues/108)) - ([a944790](https://github.com/noirbizarre/hive/commit/a94479055cbe866329b3f2195fc76ce64bb55d29))
- **usage** Stop the mock agent racing the first prompt with an idle ([#30](https://github.com/noirbizarre/hive/issues/30)) - ([0309ca3](https://github.com/noirbizarre/hive/commit/0309ca3338b613a8517eaaf261119688cc6c5f81))
- Add opt-in real-agent ACP compatibility probes - ([d1a3818](https://github.com/noirbizarre/hive/commit/d1a3818197d703c7e6e163745e1b9a3b0b22248f))

### 🏗️ Build

- **deps** Update mise dependencies ([#165](https://github.com/noirbizarre/hive/issues/165)) - ([04c5581](https://github.com/noirbizarre/hive/commit/04c55818d943070996132a3c89ca27f48f1f1b31))
- **deps** Refresh locked development tools - ([550cadb](https://github.com/noirbizarre/hive/commit/550cadb00664cda68879d4cabdf945249ea2e7bc))
- **deps** Update mise.lock and dependencies ([#121](https://github.com/noirbizarre/hive/issues/121)) - ([a047040](https://github.com/noirbizarre/hive/commit/a047040b7c8a6fe10dd4469bbdef3243517e00c2))
- **tasks** Namespace `hive:tui` - ([5dece0e](https://github.com/noirbizarre/hive/commit/5dece0ec68149a7713e34c155f15aca822a3226f))

### 🔧 CI

- Reduce Windows PR cache overhead and use ReFS test storage - ([28f9a07](https://github.com/noirbizarre/hive/commit/28f9a07cc1f73e3884b9b68a2d86318d39e891c4))
- Import Windows build storage from rust.tpl - ([4f6c617](https://github.com/noirbizarre/hive/commit/4f6c61710633694bd3c192acc50d0ac26acab753))
- Build WebUI assets once and share across jobs ([#107](https://github.com/noirbizarre/hive/issues/107)) - ([647214c](https://github.com/noirbizarre/hive/commit/647214c91e836ddcc754b04471339a00c2402bcd))

### 🧹 Chores

- **template** Merge mise v5 bootstrap ordering fix - ([a1699da](https://github.com/noirbizarre/hive/commit/a1699daf74a969c43f7709b68359104a3971470e))
- **template** Merge locked tool selection documentation - ([05efa44](https://github.com/noirbizarre/hive/commit/05efa44594e2d365b93a1eeee1488259ebde8ba8))
- **template** Merge PR cache and ReFS storage updates - ([dc80f19](https://github.com/noirbizarre/hive/commit/dc80f19103b021beb13282b42fd34725d6f08eea))
- **template** Merge coverage reporting and timeout backports - ([48df9e8](https://github.com/noirbizarre/hive/commit/48df9e813f51a594e0ac7d776e800c3e7f0da92e))
- **template** Merge latest rust.tpl improvements - ([d1eafbb](https://github.com/noirbizarre/hive/commit/d1eafbbf93a8fba1f09d3d038cd208eb1dc6cfd6))
- **template** Update rust.tpl and enable Mr Boxington - ([d378505](https://github.com/noirbizarre/hive/commit/d37850557f98c2f8dc7a70c56734f1b35e7d4305))
- Adopt rust workspace template - ([05def60](https://github.com/noirbizarre/hive/commit/05def60c1d7cfe28dd25755cc5d6d9200d4774b2))

### Tpl

- Render rust at main - ([5ca79fe](https://github.com/noirbizarre/hive/commit/5ca79fe0676215fd8b48f1fee6ac5eee81c2cae0))
- Render rust at main - ([39e705f](https://github.com/noirbizarre/hive/commit/39e705fe0e334dcb9f7d77edb175c2b6fca4a1ed))
- Render rust at main - ([898d6e7](https://github.com/noirbizarre/hive/commit/898d6e7f07cae44d388a1870684685555cc8bec0))
- Render rust at main - ([f436553](https://github.com/noirbizarre/hive/commit/f436553bcc12c74d9c6e444211e398ad074b5701))
- Render rust at main - ([1dc8d45](https://github.com/noirbizarre/hive/commit/1dc8d45794bb8225b79d2f1574b5ac14049235cc))
- Render rust at main - ([4ec98e5](https://github.com/noirbizarre/hive/commit/4ec98e5a93c72ef5af31f95cab70c6289b5a326e))
- Render rust at main - ([edb649a](https://github.com/noirbizarre/hive/commit/edb649a58f0f57b2e537063d49101af289f48fb2))
- Render rust at main - ([2aee36b](https://github.com/noirbizarre/hive/commit/2aee36b6c9553447cf2e2046a5a819b8665a159a))
- Render rust at main - ([2ec9a81](https://github.com/noirbizarre/hive/commit/2ec9a81da1a9ab356504cbb96dab0e09ce867dbe))

## ❤️ New Contributors

* @noirbizarre made their first contribution in [#170](https://github.com/noirbizarre/hive/pull/170)
