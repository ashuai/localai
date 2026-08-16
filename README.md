# localai

A Rust implementation of the **DSH-style cordis plugin architecture** for local LLMs —
the kernel only plugs and unplugs, all capabilities come from plugins.

- **Model layer**: talks to your local oMLX server (`192.168.0.5:9870`, main model
  `Qwen3.6-35b`), designed around **small contexts, zero cost, frequent micro-calls**
  (design rationale: [`docs/model-layer.md`](docs/model-layer.md));
- **Architecture**: the cordis pattern — event bus + service injection + effect
  lifecycle (unload = everything reverts) + Loader (see
  [`docs/architecture.md`](docs/architecture.md));
- **UI**: a TUI built on [ratatui](https://github.com/ratatui/ratatui); plus
  non-interactive `--once` / `--micro` modes for scripting.

## Quick start (prebuilt binary)

Download the package for your platform from
**[GitHub Releases](https://github.com/ashuai/localai/releases)** (or the **Artifacts**
of the latest run in Actions), unpack it, then:

```bash
# 1. Configure the API key once (the server's key, see server ~/.omlx/settings.json auth.api_key)
cp .env.example .env        # then edit .env and fill in LLM_API_KEY

# 2. Run the binary
./localai                   # TUI
./localai --once "hi"       # one-shot chat round (prints the reply, exits)
./localai --micro "..."     # micro-call pipeline demo (intent → parallel extraction)
./localai --list-plugins    # list plugins and load state
```

> Windows: `localai.exe`; the `.env` file sits next to the executable.
> The package contains: `localai[.exe]` + `localai.yml` + `.env.example` + `README.md`.

## Usage

- Type a message and press **Enter** → the `chat` plugin calls the local LLM;
- `/micro <text>` → micro-call pipeline demo (classify → parallel title/tags/summary);
- `/load <plugin>` `/unload <plugin>` → hot plug/unplug at runtime
  (the core cordis property: unloading reverts everything);
- `/model Qwen3.6-35b` → switch model (35b is the default main model;
  the 27b is a Claude-distilled variant, avoid unless necessary);
- `/plugins` `/help` `/clear` `/quit`; **↑/↓** browse sent history (your unsent draft is
  preserved); **PageUp/PageDown** scroll the replies; **Esc** interrupts a running call
  / clears the input box; **double Ctrl+C** quits.
- `/fs ls|cat|write|edit|stat|log [path]` — filesystem tools with a **four-layer
  permission model**: sandbox mode (L0) / workspace boundary (L1) / sensitive-file
  blocklist (L2) / read-before-edit + version guards (L3);
- `/mode [read-only|workspace-write|full]` — view/switch the sandbox mode at runtime;
- `/run <cmdline>` — subprocess execution inside the workspace root (30s timeout);
- `/pwd` — show the workspace root.

After every reply, the `microtask` plugin automatically runs 2 parallel micro-calls
(intent classification + keywords) and prints a status line like
`[micro] 意图=question | 关键词=… (1.2s)` — a live demo of zero-cost ambient calls.

## Layout

```
src/cordis/    plugin core (Context / events / Service / Plugin / Loader)
src/llm/       model layer (OpenAI-compatible client + micro-call protocol)
src/fs/        FsService + policy fence (workspace boundary, sensitive files)
src/exec/      SubprocessService (timeout, cwd-bound)
src/plugins/   built-in plugins (chat / microtask / tools / tui)
src/tui/       TUI rendering state (owned by the tui plugin)
docs/          architecture.md (cordis mapping) · model-layer.md (micro-call rationale)
changelog/     version log — the single source of truth for CI releases
```

## Build & release (GitHub Actions)

No local toolchain needed. **When it triggers** (rule aligned with the DSH project):

- **Auto-build**: pushing to `main` with changes under `changelog/**` or
  `.github/workflows/**` (i.e. a new version log, or a workflow change) triggers
  the 3-platform build + release; editing `src/`, `README.md`, etc. does **not**;
- **Manual build**: GitHub → **Actions** → **Run workflow** (`workflow_dispatch`).

**Releasing** (recommended; artifacts land on the Releases page):

```bash
cp changelog/v0.1.0.md changelog/v0.2.0.md   # write the new version log (the only step)
git add changelog && git commit -m "release: v0.2.0" && git push
```

The pipeline then: builds all 3 platforms → reads the highest version in `changelog/`
→ publishes a Release (`vX.Y.Z`, notes from the log file) only if it does not exist yet,
renaming artifacts to `localai-v<version>-<platform>.*`:
`localai-v0.1.0-win-x64.zip`, `localai-v0.1.0-macos-{aarch64,x86_64}.tar.gz`,
`localai-v0.1.0-linux-x64.tar.gz` (CentOS 7+ compatible, glibc 2.17).
Already-published versions are skipped automatically.

## Build from source (developers)

```bash
cargo run --release          # TUI
cargo run -- --once "hi"     # one-shot chat
cargo test                   # cordis core properties + loader + JSON extraction
```

## Tests

```bash
cargo test    # effect rollback on unload / event bus / services / loader / JSON extraction
```
