# Batey: Developer & AI Agent Guide

This document gives architectural context, development guidelines, and operational procedures for software engineers and AI assistants who work on **Batey**.
This file contains implementation rules for coding agents. Product scope belongs in GitHub Issues and release grouping in milestones.

## Start here

For each task:

1. Read the assigned issue/task first.
2. Inspect only the relevant source and tests; follow imports/callers outward as needed.
3. Search for existing patterns before introducing new abstractions.
4. Read `README.md` or other docs only when the task needs that context.
5. Make the smallest correct change. Avoid unrelated refactors, dependency upgrades, cleanup, or future work.

Batey is a single-owner, persistent web supervisor for local ACP (Agent Client Protocol) coding agents. It provides a web interface that follows Material 3 design and adaptive-layout conventions. It manages persistent projects, chats, ACP streaming, permissions, configuration, archive/delete, and process lifecycles.

Do not inventory the whole repository by default. Source and tests are the authority for current implementation details.

`docs/STYLE_GUIDE.md` is normative for documentation changes.

## Stack and map

Batey is a single-owner persistent supervisor for local ACP coding agents.

- Backend: Rust, `tokio`, `axum`, SQLite.
- Agent protocol: ACP over NDJSON JSON-RPC on stdio.
- Frontend: standalone Angular, TypeScript, signals, Angular Material/CDK.
- Packaging: one Rust binary embeds the production frontend with `rust-embed`.
- Builds/toolchain: Nix owns the reproducible development and release environment; `nix build .#batey` is the authoritative complete application build.

Important paths:

- `backend/src/acp/` — ACP transport, callbacks, process protocol.
- `backend/src/auth/` — agent-level authentication and its terminal PTY flows.
- `backend/src/session/` — process/session lifecycle and turn coordination.
- `backend/src/store/` — SQLite migrations and persistence domains.
- `backend/src/service/` — `HubService`, the shared user-visible application operations.
- `backend/src/web/` — HTTP/WebSocket adapters and static serving.
- `backend/src/events.rs` — event log and live/replay stream.
- `backend/fake/` — in-memory backend for frontend development.
- `frontend/src/app/` — Angular features, API clients, state, routes, components.
- `tests/` — backend integration tests; `tests/fake_acp.py` provides a fake ACP process.

## Architecture invariants

### Application boundaries

- Put user-visible Hub behavior in `HubService`, not transport handlers.
- HTTP, future MCP, federation, and other surfaces should adapt the same service operations rather than duplicate business rules.
- Keep ACP protocol/process concerns inside the ACP/session layers. `HubService` does not speak ACP directly.
- Keep provider/capability behavior explicit in agent metadata. Do not infer behavior from names such as `codex` or `claude`.

### Persistence

- `Store` owns the SQLite connection/lock; domain persistence modules operate through the established store pattern.
- Add migrations to the end of the ordered migration table. Never edit a migration that may have shipped.
- Migrations must be atomic, advance `PRAGMA user_version` only on success, and be safe when schema state is already partially/newly present.
- Never solve migration uncertainty by resetting user data.

### Events and recovery

- Preserve ordered event replay and reconnect semantics when changing event persistence or WebSocket behavior.
- Durable user work must survive process restart/reconnect unless the feature explicitly defines otherwise.

### Git workspaces

- Batey owns managed worktree creation, validation, recovery, and cleanup; ACP agents should only receive the resulting working directory.
- Never silently discard Git work. Do not implicitly reset, clean, stash, rebase, merge, fast-forward, cherry-pick, switch branches, or delete branches/worktrees containing user changes.
- Direct/project-checkout chats share the real checkout and must preserve its external state.

### Web/security

- Validate filesystem operations against configured project-root boundaries and reject symlink/path traversal.
- Git clone inputs accept only supported secure URL forms; do not introduce plain HTTP cloning.
- Redact complete URL userinfo from Git errors before returning them.
- Treat authentication, secrets, uploaded files, process execution, and remote-control surfaces as security-sensitive boundaries.

### Fake backend fidelity

When a Rust route, payload, or event contract used by the frontend changes, update `backend/fake/` in the same change. The fake backend is a frontend development surface, not proof that Rust behavior works.

## Frontend rules

- Keep strict TypeScript/template checking and zoneless compatibility.
- Use signals for reactive state and `computed()` for derived state.
- Use `inject()`, `input()`/`input.required()`, `output()`, and `model()` where appropriate.
- Use native `@if`, `@for`, and `@switch`.
- Components are standalone. Do not add NgModules or import `CommonModule` wholesale.
- Prefer focused components and stable `track` expressions for streamed collections.
- Use Angular Material/CDK instead of recreating controls.
- Avoid `::ng-deep` and Angular Material implementation selectors; use supported APIs/tokens and owned wrappers.
- Preserve compact/medium/expanded adaptive behavior and mobile-safe interaction.
- Maintain WCAG AA behavior: keyboard access, focus, contrast, accessible names, reduced-motion compatibility, and non-color status cues.
- For user-visible defects, prefer a component-level regression test that exercises the real UI behavior.

Do not introduce another frontend framework or state-management library unless an explicit tracked decision changes the architecture.

## Development and verification

Use the Nix environment rather than installing project toolchains manually:

```sh
direnv allow
# or
nix develop
```

The shell supplies `cargo`, `rustc`, `clippy`, `rustfmt`, `rust-analyzer`, `mold`, `sccache`, `cargo-nextest`, `cargo-watch`, Node 22, Python, and SQLite. It also points Cargo at the `mold` linker for the host target.

`.envrc.local` holds machine settings and stays out of git.

---

## 3. Agent Configuration (`agents.json`)

Agent definitions live in a JSON file that `--agents-file` names:

```json
{
  "opencode": { "command": "opencode", "args": ["acp"] }
}
```

Optional fields per agent:
- `args`: Array of CLI arguments.
- `env`: Key-value object of environment variables.
- `idle_timeout`: Idle timeout in seconds before the process is reaped (default: 900).
- `display_name`: Name for the user interface (default: the map key).
- `usage_provider`: Identifier of the provider that reports quota and account
  status. Batey never infers this from the agent name, so an agent named
  `codex` gets no provider until this field names one.
- `metadata`: Free-form object. Batey stores it and does not read it yet.

The file rejects an unknown field, so a typo fails at startup.

Batey's built-in catalog currently contains only local OpenCode when an
executable named `opencode` exists on Batey's runtime `PATH`. Codex ACP and
Claude ACP enter through the ACP Registry, a Batey-managed definition, or a
file or declarative definition. Batey does not detect ordinary Codex or Claude
commands, Antigravity, or other possible ACP commands.

Batey-managed custom agents and ACP Registry installs share the runtime
catalog with these declarative definitions. Their durable records live in
`installed_agents`; registry installs persist a pinned launch snapshot, so a
session never needs to contact the registry to start. Sources own their ids:
collisions are errors, custom agents are editable, registry agents use their
update/uninstall lifecycle, and file/built-in definitions are read-only.

---

## 4. Directory Structure

```
batey/
├── Cargo.toml                # Rust crate configuration (batey)
├── flake.nix                 # Nix package outputs and the dev shell
├── .envrc                    # direnv entry point for the dev shell
├── backend/
│   ├── src/                  # The Rust backend
│   │   ├── main.rs           # Binary entrypoint
│   │   ├── lib.rs            # Library exports
│   │   ├── acp/              # ACP protocol, callbacks, process supervision
│   │   ├── auth/             # Agent authentication and terminal PTY flows
│   │   ├── agents/           # Catalog, custom/installed records, Registry client and install logic
│   │   ├── service/          # HubService: the operations every surface shares
│   │   ├── session/          # Chat sessions, turn locks, idle reaping
│   │   ├── store/            # SQLite migrations, projects, chats, events
│   │   ├── events.rs         # Event log and WebSocket broadcasting
│   │   └── web/              # Axum router, REST handlers, static file serving
│   └── fake/                 # In-memory backend for frontend development
│       ├── server.mjs        # REST and WebSocket routes
│       ├── state.mjs         # Seed data and the event log
│       ├── turns.mjs         # Scripted agent turns
│       ├── websocket.mjs     # Minimal RFC 6455 server
│       └── dev.mjs           # Starts the fake backend and `ng serve`
├── static/                   # Directory placeholder (.gitkeep) populated during production Nix build
├── frontend/                 # Frontend source code
│   ├── angular.json          # Angular CLI build, serve, and test targets
│   ├── proxy.conf.json       # Dev-server proxy to the fake backend
│   ├── package.json          # Pinned frontend dependencies
│   ├── public/               # Static files copied into the Angular build
│   └── src/
│       ├── main.ts           # Frontend entrypoint
│       ├── app/              # Standalone features, services, and routes
│       └── styles.scss       # Material theme and global composition CSS
└── tests/                    # Backend integration tests
```

---

## 5. Architecture

### 5.1. Subprocess ACP Layer (`backend/src/acp/`)
- **Transport**: NDJSON JSON-RPC over standard I/O with local ACP agents.
- **Client Protocol**: Handles the ACP handshake, session initialization (`session/new`, `session/load`, `session/resume`), tool execution, plan updates, and terminal and filesystem callbacks.
- **Title Synchronization**: Watches `session_info_update` and saves the agent-generated chat title to SQLite, unless the user overrode the title.
- **Permission Callbacks**: Applies the permission policy (`ask`, `read-only`, `auto-approve`, `deny-all`) to file edits and terminal commands that the agent requests.

### 5.2. Session Lifecycle (`backend/src/session/`)
- **Process Supervision**: Spawns and supervises the ACP subprocesses on demand.
- **Turn Locking**: Allows one turn at a time per chat.
- **Process Management**: Reaps idle processes (900 seconds by default), stops them through `session/close`, and terminates the process tree when necessary.

### 5.3. Persistence (`backend/src/store/`)
- **Engine**: SQLite in WAL mode, with foreign keys and a busy timeout.
- **Migrations (`store/migrations.rs`)**: An ordered table of versioned
  migrations. Each one runs in its own transaction and advances
  `PRAGMA user_version` inside that transaction, so the version advances only
  after the migration succeeds. A database from a newer build is reported, never
  reset. Add a migration to the end of the table; never edit one that shipped.
  Write each migration so a second run is safe: prefer `IF NOT EXISTS`, and give
  it a `precondition` query when no such form exists. Batey v0.2 reset
  `user_version` on every open, so a downgraded database can arrive claiming an
  old version with a new schema. Managed chats use the `batey/chat/<chat-id>`
  branch prefix.
- **Modules**: `Store` owns the connection. `projects.rs`, `chats.rs`, and
  `events.rs` hold the SQL for one entity each and take a `&Connection`, so the
  facade controls the lock and any shared transaction.
- **Tables**:
  - `projects`: Managed repositories, with a name and a canonical path.
  - `chats`: Chats bound to a project, an agent, a title, an ACP session ID, a permission policy, and configuration values.
  - `events`: Session events in strict sequence order, with an indexed
    `session_id` column so chat deletion does not scan the table.

### 5.4. Event Dispatch and WebSockets (`backend/src/events.rs`)
- **Event Log**: A thread-safe in-memory ring buffer that holds the latest 10,000 events for reconnect and replay.
- **WebSocket Streaming**: A client subscribes with `from_seq` and resumes the stream without a gap.

### 5.5. Application Services (`backend/src/service/`)
- **`HubService`**: Owns the user-visible Hub operations for projects and chats
  and coordinates the store, the session manager, the event log, and the agent
  registry. It does not speak ACP.
- **Why**: The MCP surface and the federation surface must run the same
  operations as the browser. Put a new Hub operation here, not in a handler.
- **`ServiceError`**: Names the kind of failure (not found, invalid, conflict,
  unavailable, timeout, internal). Each transport maps it to its own errors.

### 5.5.1. Agent Authentication (`backend/src/auth/`)
- **Scope**: Authentication belongs to an installed agent, not to a chat. The
  coordinator starts its own short-lived ACP processes, so a login never
  disturbs a chat that is running a turn.
- **Inputs**: The invocation comes only from the installed `AgentRuntime` and
  from the method the agent advertised at `initialize`. A request never
  supplies an executable, an argument, a working directory, or an environment
  value.
- **Environment**: Every authentication process uses the sanitized per-agent
  environment of T108 and a Batey-owned working directory. It never resolves
  a project `.envrc`.
- **Terminal methods**: They run in a real PTY, never through ACP
  `terminal/create`. The client advertises the terminal-auth capability only
  where that PTY is active. Exit status zero means success; anything else is a
  failure.
- **Secrecy**: Terminal input and output stay in the bounded in-memory
  scrollback and on the flow socket. They never reach the store, a durable
  event, or the tracing log.

### 5.6. Web API and Static Serving (`backend/src/web/`)
- **REST Endpoints**: Projects, directory browsing, git clone, chats, ACP prompts, configuration, permissions, agent management, and agent authentication. `GET/POST /api/agents`, `POST /api/agents/validate`, registry browse/refresh/install routes, custom/registry lifecycle routes, and the `/api/agents/:id/auth`, `/api/agents/:id/logout`, and `/api/agent-auth/:flowId` routes all adapt `HubService`.
- **Adapters**: Handlers in `web/hub.rs` parse the request, call `HubService`,
  and map `ServiceError` to a status code. Business rules do not live here.
- **Web-only work (`web/git.rs`)**: Repository cloning shells out to git with
  its own timeout and cleanup. It registers the finished clone through
  `HubService` so a project row is always created one way.
- **Static Assets (`backend/src/web/static_files.rs`)**: Serves the embedded Angular assets. Hashed assets get `Cache-Control: public, max-age=31536000, immutable`. `index.html` gets revalidation headers and the History API fallback.

### 5.7. Frontend (`frontend/`)
- **Framework**: Angular standalone components with signals, `HttpClient`, the Angular Router, and RxJS for the WebSocket stream.
- **UI System**: Angular Material and CDK components, one Material 3 theme in `src/styles.scss`, and a small set of Batey status tokens.
- **Window Classes**: Compact (<600px) uses a modal drawer, full-width inputs, touch targets of 48px or more, and `env(safe-area-inset-bottom)`. Medium (600–839px) uses a modal drawer and flexible margins. Expanded (>=840px) uses a permanent drawer, a dual-pane layout, and a side sheet for configuration.
- **Routing**: `/`, `/projects/:projectId`, and `/projects/:projectId/chats/:chatId`, with the Rust SPA fallback for deep links.
- **State**: A signal store (`src/app/state/app-state.service.ts`) and a pure event reducer (`src/app/state/event-reducer.ts`) that aggregates turns, thoughts, tools, plans, and permissions.

---

## 6. Frontend Development Against the Fake Backend

`backend/fake/` serves the REST and WebSocket surface of `backend/src/web/` from memory. Use it for frontend work. It needs no Rust build, no agent binary, and no npm dependency.

```sh
cd frontend
npm run dev
```

Run the narrowest relevant checks while iterating, then the appropriate full checks before committing.

The standard final verification command for implementation tasks is:

```sh
nix run .#verify
```

It runs the canonical source-level suite with Nix-supplied tools. It covers Rust formatting, clippy, `cargo nextest run`, frontend formatting and SCSS linting, frontend tests, the frontend production build, and fake-backend tests. It works without entering `nix develop`.

Backend:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run
```

Targeted `cargo test ...` is fine during iteration. Use broader backend/integration coverage for cross-cutting persistence, session, ACP, or web changes.

Frontend:

```sh
cd frontend
npm run format
npm run format:check
npm run lint:styles
npm test
npm run build
```

`npm run format` rewrites Angular templates and SCSS with Prettier. `npm run
format:check` and `npm run lint:styles` are also part of `nix run .#verify`.

Packaging/release/deployment changes:

```sh
nix build .#batey
```

`nix build .#batey` is the authoritative complete application build, responsible for building the Angular frontend, staging assets into `static/`, and compiling the Rust binary with those assets embedded. The Rust package builds with Crane. Dependencies compile once into shared artifacts, so crate-only edits recompile only the final crate. It is packaging and release verification. It does not rerun the Rust test suite. `nix run .#verify` remains the verification path.

Do not run expensive unrelated verification solely for a docs-only or narrowly isolated change.

## Change discipline

- Add dependencies only when the task genuinely needs them; prefer existing dependencies and platform facilities.
- Preserve public/API compatibility unless the issue explicitly changes it.
- Update tests with behavior changes; do not weaken tests to make an implementation pass.
- Put implementation requirements in Issues rather than duplicating them in repository prose.
- Unless the task is explicitly an integration task on `master`, work only on the current task branch/workspace; do not switch branches, merge/rebase `master`, or push directly to `master`.
- Commit and push the current task branch after verification passes.
