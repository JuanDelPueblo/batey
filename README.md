# Batey

An uncomplicated hub to connect and manage all your ACP agents together.

No more shuffling around various tmux sessions or relying on each agent's proprietary remote control interface. Batey allows you to drive agents such as Codex, Claude Code, and OpenCode across multiple projects at the same time through a beautiful Material 3 web page that you self-host. Agents can run in parallel using separate worktrees to prevent conflicts and maximize your usage quota across each LLM provider.

## Features

- **Angular Material Adaptive UI**: A standalone Angular application using Angular Material/CDK primitives and adaptive layouts responsive to compact, medium, and expanded window sizes.
- **Persistent Projects & Chats**: Multiple independent chats per project across different or identical agents. Full process lifecycle management with automatic session resumption, cancel, stop, and reconnect.
- **Server-Side Project Creation**: Create projects by browsing existing server directories with boundary enforcement or cloning remote Git repositories directly.
- **Streamlined Chat Flow**: New chat creation lets users choose the starting local branch and workspace mode for Git projects; Batey automatically connects ACP session and displays configuration options immediately before the first prompt.
- **Dynamic Titles**: ACP agents automatically supply chat titles after conversations start, with persistent storage and optional manual rename overrides.
- **Permission & Configuration Control**: Dynamic ACP config options (grouped selects, booleans) and strict Batey permission policies (`ask`, `read-only`, `auto-approve`, `deny-all`).
- **Single-Service Architecture**: Single Rust binary embeds production-hashed frontend assets with optimized HTTP caching and WebSocket streaming.
- **Workspace Environments & Direnv Authorization**: Automatically loads authorized workspace environments via direnv (`direnv export json`) for both main project checkouts and isolated worktrees. When an `.envrc` is blocked or untrusted, Batey surfaces an inline authorization banner in the chat UI, securely invoking `direnv allow` against verified workspace paths.
- **Terminal Task Supervision**: Long-running ACP terminal commands are tracked as first-class background tasks. Users can monitor active tasks, view bounded UTF-8 output logs, and stop tasks directly from the web interface.
- **One interface for your agents** - Connect ACP-compatible agents and manage them from the same place instead of jumping between terminals and separate remote interfaces. Batey currently works with agents such as Codex, Claude Code, and OpenCode.

Batey's built-in catalog currently contains only local OpenCode when Batey
finds an executable named `opencode` on its runtime `PATH`. Codex ACP and
Claude ACP require a Registry install, a Batey-managed definition, or a file
or declarative definition.

- **Persistent projects and chats** - Organize chats under projects and come back to them later without having to recreate your setup. Batey keeps the agent process and session management behind the scenes so you can focus on the conversation.

- **Parallel worktrees** - Run multiple agents against the same Git project without making them fight over a working directory. Isolated chats get their own worktree and branch by default, allowing agents to work independently while keeping your main checkout alone.

- **Project checkout mode** - Not everything needs a worktree. Chats can work directly inside the project's existing checkout when you want an agent operating on the branch and files already there.

Prompts have no silence timeout by default, so a quiet long-running tool call is
not killed. Set `--prompt-timeout <seconds>` (or
`BATEY_PROMPT_TIMEOUT`) only when an inactivity watchdog is required.

- **A proper interface for agent work** - Follow conversations, streaming responses, tool calls, plans, permission requests, and agent state through a responsive Material 3 interface. Batey is designed for desktop and mobile layouts so your agents aren't tied to the terminal where you started them.

- **Agent configuration** - Configure available agents and their launch commands in one place while keeping per-chat options and permission policies close to the conversation. Batey talks to agents through ACP rather than maintaining a separate chat implementation for every provider.

- **Git work stays safe** - Batey treats your existing work as something it does not own. It will not silently reset, clean, stash, rebase, switch, or delete Git work to make its own job easier.

- **Self-hosted** - Run Batey on your own machine or server and put it behind the authentication and HTTPS setup you prefer. The backend only binds to localhost by default rather than exposing itself directly to the network.

## Workspace Environments & Terminal Tasks

### Direnv Integration & Authorization

Batey integrates with `direnv` to ensure ACP agent processes and terminal executions run with the expected local toolchains, environment variables, and shell configurations:
- **Automatic Resolution**: Whenever an agent process starts or creates a terminal, Batey resolves the authorized environment using `direnv export json`.
- **Security-First Authorization**: Unapproved `.envrc` files are never auto-executed or sourced directly. If direnv reports that a workspace `.envrc` is blocked, Batey catches the blocked state and surfaces a "Workspace environment blocked" banner in the web UI.
- **Strict Path Validation**: Environment authorizations only operate on paths derived from authenticated, Batey-managed chat workspace metadata, preventing path injection or traversal.

### Terminal Task Tracking

Agents that invoke long-running build, test, or watch commands via ACP terminal callbacks are supervised by Batey:
- **Active Task Monitoring**: Chat headers and cards indicate ongoing terminal tasks and keep the chat in a working status.
- **Inspection & Control**: The "Terminal tasks" dialog provides a split-view of recent and running commands, command-line arguments, working directory, exit status, and real-time output.
- **Manual Termination**: Users can terminate running background commands at any time.

## Quick start

### Running Batey

Batey currently uses Nix to provide its environment.

Enter the development environment with direnv:

```sh
direnv allow
```

or directly with Nix:

```sh
nix develop
```

Build the complete application with embedded frontend assets:

```sh
nix build .#batey
```

Run it directly from the flake without building first:

```sh
nix run . -- --project-root /path/to/projects
```

`nix run` launches the same canonical package binary that `nix build`
produces. `nix run . -- --help` shows every option.

This produces `result/bin/batey`, which includes the embedded production
frontend:

```sh
result/bin/batey \
  --project-root /path/to/projects \
  --agents-file agents.json \
  --registry-url https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json \
  --public-origin https://example.com \
  --port 9123
```

The database is optional. Without an explicit `--database` (or
`BATEY_DATABASE`), Batey uses the platform's XDG-style data location:
`$XDG_DATA_HOME/batey/batey.sqlite3` (falling back to
`~/.local/share/batey`). Managed worktrees are kept in the same Batey
path model. An explicit database retains the existing layout, with worktrees
beside its parent in `worktrees/`; no existing data is moved or removed.

The path options `--data-dir`, `--config-dir`, `--state-dir`, `--log-dir`, and
`--worktrees-dir` have matching `BATEY_*_DIR` environment variables.
`--host` / `BATEY_HOST` controls the bind address and defaults to
`127.0.0.1`; `--port` / `BATEY_PORT` controls the port.

For standalone backend development without embedded frontend assets:

```sh
cargo build --bin batey
```

This produces `target/debug/batey`:

```sh
target/debug/batey --help
```

The fake backend accepts two options:

| Option | Meaning |
| --- | --- |
| `--port <number>` | Listen port. Default 8765. |
| `--latency <factor>` | Multiplier for every simulated delay. `0.1` is fast, `3` is slow. Default 1. |

### Seeded data

The fake backend starts with three projects and ten deterministic coding-agent
fixtures. `batey` contains the idle WebSocket replay review, an actively
working reconnect investigation, an unresolved permission request, a failed
migration check, a running `nix run .#verify` terminal task, a blocked `.envrc`,
and an archived read-only migration chat. `corolla-firmware` contains a
read-only checksum review. `scratch` contains a rich screenshot/render-trace
conversation and an empty new chat. The fixtures use Claude, Codex, OpenCode,
Antigravity, and Example ACP, and include both isolated worktrees and project
checkouts. Their histories, runtime states, task record, and activity times
reset to this baseline whenever the fake backend restarts.

The waiting, working, and failed fixtures are normal ACP event histories: the
first two have an open `PROMPTING` turn (the waiting one also has an unresolved
permission request), while the failed one has an error and `stop_reason: error`.
Open the blocked fixture to see the existing workspace-environment banner, then
use **Authorize environment** to exercise the normal direnv retry path. Enable
**Show archived** to reveal the stopped archived chat.

### Prompt scenarios

A keyword in the prompt selects the turn that the fake agent streams. This makes a UI state reproducible.

| Keyword | Streamed turn |
| --- | --- |
| `plan` | Thought, then a plan that advances through its steps. |
| `tool` | Tool calls only, without a thought block. |
| `permission` | A permission request that waits for your answer. |
| `error` | An error event and a failed turn. |
| `rich` | Text, image, resource-link, resource, and rich tool content. |
| `terminal` or `task` | Starts a real fake terminal-task record that remains running after the turn. |
| `elicit` / `elicit-url` | A form or URL elicitation that waits for your answer. |
| `long` | A long answer, for scrolling and layout checks. |
| `quiet` | One short message. |
| (anything else) | A full turn: thought, plan, tool calls, permission request, and answer. |

The permission policy of the chat still applies. A chat set to `auto-approve`, `deny-all`, or `read-only` answers the request without the browser, exactly like `backend/src/acp/callbacks.rs`.

Cancel, stop, resume, archive, rename, and delete all work. Turn state, process state, and the event sequence follow the same rules as the Rust backend, so the reconnect and replay paths get exercised.

### Limits

The fake backend is a development tool. It keeps everything in memory, so a restart resets it. It has no authentication, no database, and no access to the real filesystem. It never runs an agent. Test protocol behavior against the Rust backend and the integration tests in `tests/`.

## Projects and Chats

Create a project pointing to an existing directory under a configured project root or clone from a Git repository. Canonical paths reject missing directories and symlink escapes. Create as many chats as needed, including several using the same agent in one project. Each chat has a stable UUID, independent process, ACP session ID, turn lock, permission policy, and selected ACP configuration values.

### Git chat workspaces

Git chats default to **Isolated worktree**. When creating a chat, choose the
starting local branch; Batey creates a deterministic branch named
`batey/chat/<chat-id>` and a separate managed worktree. Uncommitted changes
in the primary checkout are not copied into it. The branch and workspace
identity are visible in the chat header and configuration panel.

Users may explicitly choose **Project checkout**, which operates on the real
project checkout and the selected branch. Switching that checkout is allowed
only when it is safe (clean and not in use). Direct and legacy chats share the
same repository checkout turn lock and therefore cannot work concurrently.
Batey never silently switches a direct chat back to its expected branch on
resume.

Deleting a clean managed chat removes its worktree but retains its branch.
Deletion refuses to discard dirty or untracked managed-worktree files. Direct
and legacy chat deletion leaves the repository checkout and Git state alone.

Opening a chat automatically connects the agent process and loads ACP configuration options immediately. Sending a prompt also connects automatically if stopped. `session/load` or advertised `session/resume` restores agent-owned conversation state.

Stop process and idle reaping preserve the chat and ACP session ID. Batey
only reaps an idle process when its ACP agent advertises `session/load` or
`session/resume`; otherwise it keeps the process alive so the chat remains
usable. The default eligible idle timeout is 900 seconds. Cancel turn sends
`session/cancel` and resolves pending browser permissions. Archive stops an idle
process and keeps metadata; Restore makes the chat usable again. Delete removes
local metadata/activity only, never project files or the agent's own session
history.

## Configuration

Agents from a file are configured in `agents.json`:

```json
{
  "opencode": {
    "command": "opencode",
    "args": ["acp"]
  }
}
```

Definitions accept optional `args` (array), `env` (object), `idle_timeout`
(seconds), `display_name` (string), `usage_provider` (string), and `metadata`
(object). Batey never derives a usage provider from the agent name.
The server binds to `127.0.0.1`. Put authenticated HTTPS in front of it before exposing Batey remotely.

`--registry-url` (or `BATEY_REGISTRY_URL`) selects the HTTPS ACP Registry
catalog. Registry installs store a pinned launch snapshot locally; browsing or
refreshing the catalog is never required to resume a chat.

The agent-management API is provider-neutral:

- `GET` / `POST /api/agents`, `PATCH` / `DELETE /api/agents/:id`, and `POST /api/agents/validate` manage custom definitions.
- `GET /api/agents/registry`, `POST /api/agents/registry/refresh`, and `POST /api/agents/registry/install` browse and install registry entries.
- `POST /api/agents/:id/update` updates an installed registry agent.

Built-in and `agents.json` definitions are read-only through this API. Agent
summaries report source, availability, mutability, display metadata, and an
unavailable reason when applicable; launch commands and environment values are
never returned.

Agent authentication is provider-neutral too, and it belongs to the agent
rather than to a chat:

- `GET /api/agents/:id/auth` reports the methods the agent advertised at
  `initialize`, whether it supports logout, and whether this build runs
  terminal authentication. A method type Batey cannot run comes back as
  unsupported; Batey never guesses a fallback for it. The view also carries
  `active_flow` when one unfinished flow exists, so a reload can resume or
  cancel it. The active-flow summary holds only a flow id, a kind, a method
  id, a lifecycle state, and a start time. It never carries PTY output,
  credentials, tokens, device codes, or sensitive URLs.
- `POST /api/agents/:id/auth/:methodId` runs an `agent` method through the
  stable `authenticate` request.
- `POST /api/agents/:id/logout` runs the stable `logout` request. It goes out
  only when the agent advertised that capability, and it never touches Batey
  Hub chats, sessions, or history.
- `POST /api/agents/:id/auth/terminal/:methodId` starts a `terminal` method in
  a real PTY and returns a flow. `GET /api/agent-auth/:flowId`,
  `POST /api/agent-auth/:flowId/cancel`, and `GET /api/agent-auth/:flowId/ws`
  read, cancel, and drive that flow.

A terminal flow reproduces the configured agent invocation: the same
executable, the same arguments with the advertised ones appended, the same
sanitized environment with the advertised values overriding it, and a
Batey-owned working directory. No request supplies an executable, an
argument, a working directory, or an environment value, so the API cannot
become a remote shell. Terminal input and output stay in memory: they never
reach the event log, the database, or the server log. Cancelling a flow, a
flow nobody watches, and server shutdown all kill the whole process tree.

An agent that answers `auth_required` produces a recoverable `409` with
`"code": "auth_required"` and the agent id. The chat and its history stay
exactly as they were.

`observed_state` is provider-neutral evidence. `unknown` means ACP supplied
no evidence yet. Batey shows the available methods normally and makes no
signed-in or signed-out claim. Once Batey observes `authenticated`, it hides
the sign-in methods and shows **Log out** when the agent supports logout.
Logout or `auth_required` restores the sign-in methods.

### Agent authentication in containers

Batey's backend has no ordinary browser. Upstream agents differ in how they
authenticate in a container. Batey centralizes these defaults in agent
metadata. A T131 per-agent environment override always wins.

- **Codex** - Batey sets `NO_BROWSER=1` for the Codex authentication probe
  and protocol-auth processes. Codex then offers the working ChatGPT
  device-code method. It does not offer the ordinary local-browser method.
  Batey never forces `NO_BROWSER` on ordinary Codex chat sessions.
- **GitHub Copilot** - Batey sets `CI=true` for the Copilot
  authentication and terminal-auth processes. Upstream Copilot then chooses
  its headless/device-code path instead of a loopback-browser callback.
  Batey never forces `CI` on ordinary Copilot chat sessions, and it never
  appends `--device-code` to the ACP-advertised arguments.
- **OpenCode** - Use the terminal method to run `opencode auth login` for
  additional providers. A Registry-installed OpenCode resolves its own
  installed executable. Batey does not add `/data/agents/**` to `PATH`.
- **Antigravity** - Use API-key authentication with `GEMINI_API_KEY`. The
  interactive browser/Google path may need a localhost callback that ACP does
  not currently expose in a fully remote-friendly way. Batey shows that
  warning next to the affected method only. One-time interactive workaround:
  authenticate Antigravity inside the same persistent Batey environment, use
  the upstream remote/SSH-friendly flow when the tool offers one, forward or
  publish the localhost callback port shown by the tool to the machine that
  runs the browser, and keep `/data` persistent so the credentials survive
  container recreation. Batey does not implement Google's OAuth flow and
  never guesses a fixed callback port.

A protocol-authentication timeout reports a provider-neutral message. It
states that the agent may need a browser or an interactive environment that
the agent did not expose through ACP. Cancel stays available throughout the
wait, and the user interface never stays at **Signing in** or
**Checking sign-in**.

Deployments can also supply `--declarative-agents-file` (or
`BATEY_DECLARATIVE_AGENTS_FILE`). It has the same shape as `agents.json`
plus `pass_env`, `default_permission_policy`, and `description`, feeds the
same catalog with `AgentSource::Declarative`, and stays read-only in the
management APIs. An `npx` or `uvx` agent is just a pinned manual launch such
as `command = "npx"` with `args = ["--yes", "package-acp@1.2.3", "--acp"]`,
so no live registry lookup happens. The NixOS module generates this file for
you; see the production deployment section below.

## Production deployment

Batey behaves like a normal flake package and NixOS service.
Another flake can consume it directly without copying packaging code. The OCI
image is built from exactly the same package.

### Flake outputs

```sh
nix build                 # same as nix build .#batey
nix run . -- --help       # runs packages.default, the canonical package
nix build .#batey-oci     # OCI image tarball
```

The supported package outputs are `packages.${system}.batey` and
`packages.${system}.default`, which are the same canonical Crane build, plus
`packages.${system}.batey-oci` for the OCI image. The production frontend is
an internal input to the Batey package, not a separate public package. The
version comes from `Cargo.toml`, so there is one authoritative source.

To override the service package, point it at the canonical build explicitly:

```nix
services.batey.package = batey.packages.${system}.batey;
```

### Minimal NixOS configuration

```nix
{
  inputs.batey.url = "github:JuanDelPueblo/batey";
  outputs = { nixpkgs, batey, ... }: {
    nixosConfigurations.server = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        batey.nixosModules.default
        {
          services.batey.enable = true;
          services.batey.projectRoots = [ "/srv/projects" ];
        }
      ];
    };
  };
}
```

That is the whole thing for a standard deployment. Importing
`batey.nixosModules.default` defaults `services.batey.package` to the exact
canonical Batey package from this flake; no overlay is required. Override it
only when you want a different build. Add network options only
when you need them, such as `services.batey.host`, `.port`, or
`.publicOrigin`. The module also exposes typed options for prompt timeout,
registry URL, data/state/config/log/worktree locations, runtime packages,
environment values, environment files, and declarative agents. Run
`nixos-option services.batey` to browse them.

The service starts at the normal multi-user target, restarts on failure,
shuts down gracefully on SIGTERM, and cleans up supervised ACP descendants
through its control group, so no agent processes are left behind after a
stop or restart. Sandboxing is intentionally light: agents must still work
in project roots, use Git, start terminal tasks, and resolve workspace
environments.

Nix workspace environments work out of the box. The service PATH provides
`direnv` and the Nix tooling needed for `use flake` and nix-direnv style
`.envrc` files without a custom Batey package, and the service sets
`NIX_CONFIG=experimental-features = nix-command flakes` for its own
environment, so no system-wide Nix settings are required. Batey never
auto-authorizes `.envrc` files. When an environment is blocked, authorize
it from the chat UI, which runs `direnv allow` against the verified
workspace path.

Supported registry `npx` and `uvx` agents work with the generic runtimes in
`services.batey.runtimePackages`, which defaults to Node (`npx`) and
`uv` (`uvx`). Extend or override that list for your deployment, but do not
expect project toolchains there. Remove an entry and its agents are reported
deterministically unavailable instead of failing at session start.

### Service user and paths

By default the module creates a dedicated `batey` system user and group
and gives it a stable HOME at `/var/lib/batey`. Agent authentication
and configuration stored there survives restarts and package upgrades.
Persistent Batey state lives under systemd directory management
(`StateDirectory=batey`), with deterministic `--data-dir`,
`--state-dir`, and `--config-dir` instead of root's HOME. Redirecting them
needs no manual setup either: the module creates and chowns every
Batey-owned directory through tmpfiles, so `dataDir = "/srv/batey-data"`
just works. Project roots are the exception on purpose — they hold your
data, so the service never takes ownership of them.

To run as an existing account instead:

```nix
services.batey.user = "alice";
services.batey.group = "users";
```

An explicitly selected user or group is assumed to exist and is never
redefined. Give that account read and write access to every entry in
`services.batey.projectRoots`, and make sure its HOME persists if your
agents keep auth there.

### Declarative agents

```nix
services.batey.agents.my-agent = {
  command = "${pkgs.my-agent}/bin/my-acp";
  args = [ "--stdio" ];
  displayName = "My agent";
  idleTimeout = 300;
  usageProvider = "internal";
  defaultPermissionPolicy = "ask";
  description = "Our own agent";
  env.REGION = "eu";
  passEnv = [ "MY_AGENT_TOKEN" ];
};
services.batey.agents.pkg = {
  npx.package = "package-acp@1.2.3";
  npx.args = [ "--acp" ];
};
```

Each agent sets exactly one of `command`, `npx`, or `uvx`. A Nix package
path works naturally as `command`. Pinned `npx` (`pkg@1.2.3`) and `uvx`
(`pkg==1.2.3`) launches are reproducible and never fetch a mutable
`latest` entry during evaluation or startup. Binary registry installs are
covered the same way: point `command` at a Nix-provided store path with an
explicit source instead of downloading latest. Declarative agents appear
with source `declarative`, stay read-only in the web management APIs, and
live happily beside web-managed custom and registry agents. An id that any
other source already owns fails clearly at startup instead of silently
winning.

### Secrets and environment files

Never put API keys, tokens, or passwords in `env`. Those values land in the
Nix store. Name them with `passEnv` and supply the values at runtime:

```nix
services.batey.environmentFiles = [ "/run/secrets/batey.env" ];
services.batey.agents.my-agent.passEnv = [ "MY_AGENT_TOKEN" ];
```

Where `/run/secrets/batey.env` holds `MY_AGENT_TOKEN=...`. Use
`services.batey.environment` only for non-secret values.

The boundary is real, not advisory. At startup Batey moves every listed
secret name out of its own environment into a stash, so the inherited
workspace environment that all agents share never carries them. Each value
is then injected only into the agents whose `passEnv` names it. An agent
that names nothing — including every web-managed custom agent — receives
no secret, and one agent never sees another agent's token. Names that
should be stripped but injected nowhere belong in
`services.batey.secretEnvVars`.

### Containers

#### Local OCI test

Use the local Compose workflow to test the production OCI image. It does not
install or activate `services.batey`, use Batey's normal host XDG state, or
require `nixos-rebuild`.

Run the helper from the repository root:

```sh
nix/load-local-image.sh
```

The helper builds `.#batey-oci`, loads its archive into Docker or Podman, finds
the versioned image, tags it as `batey:local`, creates `.batey-docker`, and
records the invoking UID and GID for Compose. Docker takes priority when both
Docker and Podman exist.

Start the local test instance:

```sh
docker compose up -d
```

Open `http://localhost:8765`. Inspect logs with `docker compose logs -f`.
Stop the instance with `docker compose down`. Erase all local test state with
`rm -rf .batey-docker`.

Use the [Registry OCI test](docs/registry-oci-test.md) to verify the production
Registry, local search, refresh, and the durable cache.

The default project mount is `./.batey-docker/projects:/projects`. Add a
disposable Git repository there for the first test. To mount a development
project root, set `BATEY_PROJECT_ROOT` explicitly:

```sh
BATEY_PROJECT_ROOT="$PWD/my-projects" docker compose up -d
```

The override must name a host directory that the container can access. A real
project mount gives containerized agents write access to that project. Use
disposable projects for the safer first test.

The Compose workflow uses only `.batey-docker/data` for Batey state. It does not
touch host XDG data, config, or state directories. The helper creates a root
`.env` file with only the numeric UID and GID that Compose needs; Git ignores
that file. Removing `.batey-docker` resets the local Batey instance.

Podman users can run the same commands with `podman compose` when their Podman
installation provides Compose compatibility.

This manual workflow does not replace the lower-level OCI smoke test. The
automated test remains in `nix/oci-smoke.sh`.

#### Generic container commands

```sh
nix build .#batey-oci
docker load -i result  # prints the tag, e.g. batey:0.4.0
docker run --rm -p 127.0.0.1:8765:8765 \
  -v batey-data:/data \
  -v "$PWD/projects:/projects" \
  batey:0.4.0
```

The image is built with `dockerTools` from the canonical package, not a
second compiler path, and there is no Dockerfile to drift. It runs non-root,
binds `0.0.0.0:8765`, keeps HOME and state in `/data`, and expects projects
in `/projects`:

```sh
docker load -i result
docker run --rm -p 127.0.0.1:8765:8765 \
  -v batey-data:/data \
  -v "$PWD/projects:/projects" \
  batey:latest
```

Persist `/data` if you want the database and ACP-agent auth to survive
container replacement. A named volume inherits the image's non-root
ownership and just works; a host bind mount must be writable by UID 65534
(the image user). Mount each project root under `/projects` (or pass
your own `--project-root` flags plus matching mounts). A runtime smoke test
lives in `nix/oci-smoke.sh` and checks `/api/status` plus the embedded
frontend with temporary mounts.

### Developing Batey

Clone the repository and enter its Nix environment:

```sh
git clone https://github.com/JuanDelPueblo/batey.git
cd batey
direnv allow
```

The frontend can be developed independently:

```sh
cd frontend
npm run dev
```

Then open `http://localhost:4200`.

The standard final check for any implementation task is:

```sh
nix run .#verify
```

It runs the full source-level suite — Rust formatting, clippy, Rust tests, frontend tests, the frontend production build, and fake-backend tests — using tools supplied by Nix, so you don't need to enter `nix develop` first.

Before submitting backend changes:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run
```

For frontend changes:

```sh
cd frontend
npm test
npm run build
```

The complete application with embedded frontend assets is built authoritatively with:

```sh
nix build .#batey
```

This build is packaging and release verification. It does not rerun the Rust test suite. `nix run .#verify` remains the verification path.

## License

GPL-3.0-only.

Backend forked from github.com/missdeer/ccgonext.

Batey does not include proprietary agent binaries.
