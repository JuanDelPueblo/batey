// In-memory replacement for the SQLite store of `backend/src/store.rs`.
// The shapes match `store::Project`, `store::Chat` and `events::SessionEvent`.

import { randomUUID } from 'node:crypto';

// This is the fake equivalent of the real provider-neutral /api/agents
// catalog. Keep metadata explicit rather than deriving it from the id.
export const AGENTS = [
  { id: 'antigravity', display_name: 'Antigravity', source: 'builtin', availability: 'available', usage_provider: null, metadata: {}, mutability: 'read_only', display: {} },
  { id: 'claude', display_name: 'Claude', source: 'builtin', availability: 'available', usage_provider: null, metadata: {}, mutability: 'read_only', display: { description: 'Anthropic Claude ACP agent.', version: '5.0.0' } },
  { id: 'codex', display_name: 'Codex', source: 'builtin', availability: 'available', usage_provider: null, metadata: {}, mutability: 'read_only', display: { description: 'OpenAI Codex ACP agent.', version: '5.1.0' } },
  { id: 'opencode', display_name: 'OpenCode', source: 'builtin', availability: 'available', usage_provider: null, metadata: {}, mutability: 'read_only', display: {} },
  {
    id: 'legacy-file',
    display_name: 'Legacy File Agent',
    source: 'file',
    availability: 'available',
    usage_provider: null,
    metadata: {},
    mutability: 'read_only',
    display: { description: 'Defined by the --agents-file source.', version: '0.9.0' },
  },
  {
    id: 'nix-agent',
    display_name: 'Nix Declarative Agent',
    source: 'declarative',
    availability: 'available',
    usage_provider: null,
    metadata: {},
    mutability: 'read_only',
    display: { description: 'Supplied by the declarative deployment source.' },
  },
  {
    id: 'broken-agent',
    display_name: 'Broken Agent',
    source: 'builtin',
    availability: 'unavailable',
    unavailable_reason: 'The configured command is not installed on this host.',
    usage_provider: null,
    metadata: {},
    mutability: 'read_only',
    display: {},
  },
  {
    id: 'example-acp',
    display_name: 'Example ACP',
    source: 'registry',
    registry_id: 'example-acp',
    availability: 'available',
    usage_provider: null,
    metadata: {},
    mutability: 'registry_managed',
    display: { description: 'A representative ACP Registry entry for frontend development.', version: '1.0.0' },
  },
];

// The fake ACP Registry catalog. `unsupported_reason` mirrors a host that
// cannot install an entry; the frontend must present it honestly.
export const REGISTRY_ENTRIES = [
  {
    id: 'example-acp',
    name: 'Example ACP',
    version: '1.2.0',
    description: 'A representative ACP Registry entry for frontend development.',
    repository: 'https://example.invalid/example-acp',
    website: 'https://example.invalid',
    authors: ['Example Author'],
    license: 'MIT',
    distributions: ['npx'],
    platforms: [],
    selected_distribution: 'npx',
  },
  {
    id: 'native-agent',
    name: 'Native Agent',
    version: '2.0.0',
    description: 'A binary ACP agent for this host.',
    repository: 'https://example.invalid/native-agent',
    authors: ['Native Author'],
    license: 'Apache-2.0',
    distributions: ['binary'],
    platforms: ['linux-x86_64'],
    selected_distribution: 'binary',
  },
  {
    id: 'windows-only',
    name: 'Windows Only',
    version: '1.0.0',
    description: 'A binary ACP agent that this host cannot install.',
    distributions: ['binary'],
    platforms: ['windows-x86_64'],
    selected_distribution: null,
    unsupported_reason: 'No binary distribution covers this platform.',
  },
];

/** Provider-neutral authentication state, keyed by agent id. */
function defaultAuth(agentId) {
  return { agent_id: agentId, authenticated: false, methods: [] };
}

export const AUTH_METHODS = {
  claude: {
    logout_supported: true,
    methods: [
      { id: 'claude-oauth', name: 'Sign in with Claude', type: 'agent', description: 'Open the provider sign-in page.', supported: true },
    ],
  },
  codex: {
    logout_supported: true,
    methods: [
      { id: 'openai-oauth', name: 'Sign in with OpenAI (device code)', type: 'agent', description: 'ChatGPT device-code sign-in for a headless backend.', supported: true },
      { id: 'api-key', name: 'API key', type: 'terminal', description: 'Enter an API key in a terminal.', supported: true },
    ],
  },
  opencode: {
    logout_supported: false,
    methods: [
      { id: 'opencode-oauth', name: 'OAuth', type: 'agent', description: null, supported: true },
      { id: 'device-code', name: 'Legacy device flow', type: 'device_code', description: null, supported: false },
    ],
  },
  'example-acp': {
    logout_supported: false,
    methods: [
      { id: 'example-token', name: 'Example token', type: 'terminal', description: null, supported: true },
    ],
  },
  antigravity: {
    logout_supported: false,
    methods: [
      {
        id: 'antigravity-interactive',
        name: 'Interactive sign-in',
        type: 'agent',
        description: 'Complete the interactive step, or set GEMINI_API_KEY for API-key auth.',
        supported: true,
        warning: 'Upstream Antigravity sign-in may need a browser or a localhost callback that ACP does not expose in a fully remote-friendly way. API-key auth still works when GEMINI_API_KEY is set for this agent. One-time interactive workaround: run the login inside this same persistent Batey environment, use the upstream remote/SSH-friendly flow when the tool offers one, forward or publish the localhost callback port shown by the tool to the machine running the browser, and keep /data persistent so the credentials survive container recreation.',
      },
    ],
  },
  'opencode-legacy': {
    logout_supported: false,
    methods: [
      { id: 'opencode-login', name: 'Log in with OpenCode', type: 'terminal', description: 'Run `opencode auth login` in the terminal', supported: true },
    ],
  },
  copilot: {
    logout_supported: true,
    methods: [
      { id: 'copilot-login', name: 'Log in with Copilot CLI', type: 'terminal', description: 'Run `copilot login` in the terminal', supported: true },
    ],
  },
};

export const PERMISSION_POLICIES = ['ask', 'read-only', 'auto-approve', 'deny-all'];

/** Root of the synthetic directory tree that the folder picker browses. */
export const PROJECT_ROOT = '/home/dev/projects';

// Small, valid ACP content used by the rich-history fixture and the `rich`
// prompt scenario. It is intentionally self-contained: the fake backend
// never reads files or fetches remote resources.
export const RICH_IMAGE_DATA = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=';
export const RICH_HISTORY_CONTENT = [
  { type: 'text', text: 'The captured dashboard state after the Material 3 migration.' },
  { type: 'image', data: RICH_IMAGE_DATA, mimeType: 'image/png', uri: 'https://example.invalid/batey/dashboard.png' },
  {
    type: 'resource_link',
    name: 'Render trace',
    title: 'Open the render trace',
    description: 'A safe, metadata-only link from the fake ACP agent.',
    uri: 'https://example.invalid/batey/render-trace.json',
    mimeType: 'application/json',
  },
  {
    type: 'resource',
    resource: {
      uri: 'https://example.invalid/batey/artifacts/render-trace.json',
      mimeType: 'application/json',
      text: '{"route":"/projects/batey","firstPaintMs":184,"layoutShift":0.01}',
    },
  },
];

function now() {
  return new Date().toISOString();
}

/** Config options in the shape the ACP agents advertise. */
export function defaultConfigOptions(agent) {
  return [
    {
      id: 'model',
      name: 'Model',
      type: 'select',
      currentValue: agent === 'codex' ? 'gpt-5-codex' : 'claude-opus-5',
      description: 'Model that answers in this chat.',
      options: [
        {
          group: 'Anthropic',
          options: [
            { value: 'claude-opus-5', name: 'Claude Opus 5' },
            { value: 'claude-sonnet-5', name: 'Claude Sonnet 5' },
            { value: 'claude-haiku-4-5', name: 'Claude Haiku 4.5' },
          ],
        },
        {
          group: 'OpenAI',
          options: [
            { value: 'gpt-5-codex', name: 'GPT-5 Codex' },
            { value: 'gpt-5', name: 'GPT-5' },
          ],
        },
        {
          group: 'Local',
          options: [{ value: 'qwen-coder', name: 'Qwen Coder 32B' }],
        },
      ],
    },
    {
      id: 'reasoning_effort',
      name: 'Reasoning effort',
      type: 'select',
      currentValue: 'medium',
      description: 'Time the agent spends before it answers.',
      options: [
        { value: 'low', name: 'Low' },
        { value: 'medium', name: 'Medium' },
        { value: 'high', name: 'High' },
      ],
    },
    {
      id: 'web_search',
      name: 'Web search',
      type: 'boolean',
      currentValue: false,
      description: 'Let the agent read pages from the web.',
    },
    {
      id: 'auto_commit',
      name: 'Commit after every turn',
      type: 'boolean',
      currentValue: true,
      description: 'Write a git commit when a turn ends.',
    },
  ];
}

export function defaultCommands() {
  return [
    { name: 'plan', description: 'Create an implementation plan', input: { hint: 'goal for the plan' } },
    { name: 'review', description: 'Review the current changes', input: null },
  ];
}

export function defaultModes() {
  return {
    current_mode_id: 'ask',
    available_modes: [
      { id: 'ask', name: 'Ask', description: 'Ask before acting' },
      { id: 'act', name: 'Act', description: 'Act without asking' },
    ],
  };
}

export function defaultUsage() {
  return { used: 1200, size: 200000, cost_amount: 0.012, cost_currency: 'USD' };
}

export class FakeState {
  constructor() {
    this.projects = new Map();
    this.chats = new Map();
    // The real backend stores this sequence in SQLite. The fake keeps the
    // server-owned sequence for its process lifetime so deleted chats never
    // make a default title available for reuse.
    this.nextChatNumber = 1;
    this.configByChat = new Map();
    this.mcpByChat = new Map();
    this.additionalRootsByChat = new Map();
    this.commandsByChat = new Map();
    this.modesByChat = new Map();
    this.usageByChat = new Map();
    this.elicitationsByChat = new Map();
    this.remoteSessionsByChat = new Map();
    this.workspaceOptionsByProject = new Map();
    // Live process state, which the real backend holds in the session manager.
    this.runtime = new Map();
    this.tasksByChat = new Map();
    this.blockedChats = new Set();
    // Project-level "remember for project" direnv grants (T125): project id
    // -> { relativePath, contentHash }. Mirrors the real backend's
    // `project_envrc_grants` table.
    this.projectEnvrcGrants = new Map();
    // Internal startup instructions consumed by the fake turn runner. These
    // are not part of any HTTP response; they keep seeded open turns alive
    // after their durable event history has been written.
    this.seededTurns = new Map();

    this.events = [];
    this.nextSeq = 1;
    this.listeners = new Set();

    // Authentication state and opaque terminal flows (T111 contract).
    // `observedByAgent` is in-memory observed evidence: unknown on a fresh
    // process, never a durable authenticated boolean.
    this.authByAgent = new Map();
    this.observedByAgent = new Map();
    this.flows = new Map();
    this.protocolFlows = new Map();
    this.protocolElicitations = new Map();
    // Editable Batey-managed definitions, including their launch environment.
    this.customDetails = new Map();
    // Private per-agent environment overrides: agent id -> Map(name -> value).
    // Presence views never expose values, mirroring the Rust redaction.
    this.agentEnv = new Map();
    this.registryFetched = false;
    this.registryFetchedAt = null;
    this.registryRefreshError = null;

    this.seed();
  }

  // ---------------------------------------------------------------- events

  /** Appends an event and publishes it, exactly like `EventLog::append`. */
  emit(sessionId, agent, payload, timestamp = now()) {
    const event = {
      seq: this.nextSeq,
      timestamp,
      session_id: sessionId,
      agent,
      payload,
    };
    this.nextSeq += 1;
    this.events.push(event);
    for (const listener of this.listeners) listener(event);
    return event;
  }

  /** Tells the browser that projects or chats changed. */
  metadataChanged(timestamp = now()) {
    this.emit('', '', { type: 'metadata_changed' }, timestamp);
  }

  replayFrom(fromSeq) {
    return this.events.filter((event) => event.seq >= fromSeq);
  }

  historyPage(chatId, beforeSeq, limit = 100, throughSeq) {
    const history = this.events
      .filter((event) => event.session_id === chatId
        && (beforeSeq == null || event.seq < beforeSeq)
        && (throughSeq == null || event.seq <= throughSeq))
      .sort((a, b) => b.seq - a.seq);
    const page = history.slice(0, limit);
    const hasOlder = history.length > limit;
    page.reverse();
    return {
      events: page,
      next_cursor: hasOlder ? page[0]?.seq ?? null : null,
      has_older: hasOlder,
    };
  }

  subscribe(listener) {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  forgetChat(chatId) {
    this.events = this.events.filter((event) => event.session_id !== chatId);
    this.tasksByChat.delete(chatId);
    this.blockedChats.delete(chatId);
    this.commandsByChat.delete(chatId);
    this.modesByChat.delete(chatId);
    this.usageByChat.delete(chatId);
    this.elicitationsByChat.delete(chatId);
    this.remoteSessionsByChat.delete(chatId);
    this.configByChat.delete(chatId);
    this.mcpByChat.delete(chatId);
    this.additionalRootsByChat.delete(chatId);
  }

  // -------------------------------------------------------------- projects

  createProject(name, path, workspaceOptions = null) {
    const project = {
      id: randomUUID(),
      name,
      path,
      created_at: now(),
      updated_at: now(),
      chat_count: 0,
    };
    this.projects.set(project.id, project);
    this.workspaceOptionsByProject.set(project.id, workspaceOptions ?? {
      is_git: false, current_branch: null, head_sha: null, dirty: false, branches: [],
    });
    return project;
  }

  workspaceOptions(projectId) {
    const options = this.workspaceOptionsByProject.get(projectId);
    if (!options) return null;
    return { ...options, branches: options.branches.map((branch) => ({ ...branch })) };
  }

  projectView(project) {
    const chatCount = [...this.chats.values()].filter(
      (chat) => chat.project_id === project.id,
    ).length;
    const grant = this.projectEnvrcGrants.get(project.id) ?? null;
    return {
      ...project,
      chat_count: chatCount,
      envrc_remembered: grant != null,
      envrc_relative_path: grant?.relativePath ?? null,
    };
  }

  listProjects() {
    return [...this.projects.values()].map((project) => this.projectView(project));
  }

  // ----------------------------------------------------------------- chats

  createChat(projectId, agent, title, workspace) {
    const hasExplicitTitle = typeof title === 'string' && title.trim().length > 0;
    const finalTitle = hasExplicitTitle ? title.trim() : `New chat ${this.nextChatNumber++}`;
    const chat = {
      id: randomUUID(),
      project_id: projectId,
      agent,
      title: finalTitle,
      acp_session_id: null,
      created_at: now(),
      updated_at: now(),
      archived: false,
      config_values: {},
      title_overridden: hasExplicitTitle,
      workspace: workspace ? {
        mode: workspace.mode,
        branch: workspace.mode === 'managed_worktree' ? `batey/chat/${chat.id}` : workspace.branch,
        base_commit: workspace.base_commit ?? this.workspaceOptionsByProject.get(projectId)?.branches
          ?.find((branch) => branch.name === workspace.branch)?.sha ?? null,
      } : null,
    };
    this.chats.set(chat.id, chat);
    this.configByChat.set(chat.id, defaultConfigOptions(agent));
    this.mcpByChat.set(chat.id, []);
    this.additionalRootsByChat.set(chat.id, []);
    this.commandsByChat.set(chat.id, defaultCommands());
    this.modesByChat.set(chat.id, defaultModes());
    this.usageByChat.set(chat.id, defaultUsage());
    this.elicitationsByChat.set(chat.id, []);
    this.remoteSessionsByChat.set(chat.id, [
      { sessionId: `acp-${chat.id.slice(0, 8)}`, title: finalTitle },
      { sessionId: 'acp-older-session', title: 'Earlier conversation' },
    ]);
    this.runtime.set(chat.id, { process: 'STOPPED', turn: 'IDLE' });
    return chat;
  }

  /** Applies an ACP-generated title unless a manual rename already won. */
  updateGeneratedTitle(chat, title) {
    if (chat.title_overridden || chat.title === title) return false;
    chat.title = title;
    chat.updated_at = now();
    this.metadataChanged();
    return true;
  }

  /** Adds the live process fields, like `hub::chat_view`. */
  chatView(chat) {
    const runtime = this.runtime.get(chat.id) ?? { process: 'STOPPED', turn: 'IDLE' };
    const tasks = this.tasksByChat.get(chat.id) ?? [];
    const activeTasks = tasks.filter((task) => task.state === 'running').length;
    return {
      ...chat,
      turn_started_at: this.activeTurnStartedAt(chat.id),
      process_state: runtime.process,
      turn_state: runtime.turn,
      active_tasks: activeTasks,
    };
  }

  isEnvironmentBlocked(chatId) {
    return this.blockedChats.has(chatId);
  }

  blockEnvironment(chatId) {
    this.blockedChats.add(chatId);
  }

  /** Blocks a newly created chat's environment, unless its project already
   * has a matching remembered grant (T125) — mirrors the real backend
   * auto-allowing a new managed worktree whose .envrc matches. */
  blockEnvironmentUnlessRemembered(chatId, projectId) {
    if (!this.hasProjectEnvrcGrant(projectId)) this.blockEnvironment(chatId);
  }

  authorizeEnvironment(chatId, remember = false) {
    this.blockedChats.delete(chatId);
    if (remember) {
      const chat = this.chats.get(chatId);
      if (chat) {
        this.rememberProjectEnvrc(chat.project_id, {
          relativePath: '.envrc',
          contentHash: 'fixture-hash',
        });
      }
    }
  }

  hasProjectEnvrcGrant(projectId) {
    return this.projectEnvrcGrants.has(projectId);
  }

  rememberProjectEnvrc(projectId, { relativePath, contentHash }) {
    this.projectEnvrcGrants.set(projectId, { relativePath, contentHash });
  }

  forgetProjectEnvrc(projectId) {
    this.projectEnvrcGrants.delete(projectId);
  }

  listTasks(chatId) {
    const tasks = this.tasksByChat.get(chatId) ?? [];
    return tasks.map(({ output, truncated, ...summary }) => ({ ...summary }));
  }

  getTask(chatId, taskId) {
    const tasks = this.tasksByChat.get(chatId) ?? [];
    const task = tasks.find((t) => t.id === taskId);
    return task ? { ...task } : null;
  }

  stopTask(chatId, taskId) {
    const tasks = this.tasksByChat.get(chatId) ?? [];
    const task = tasks.find((t) => t.id === taskId);
    if (!task) return false;
    if (task.state === 'running') {
      task.state = 'stopped';
      task.completed_at = now();
      this.metadataChanged();
    }
    return true;
  }

  createTask(chatId, command, cwd, initialOutput = '', startedAt = now()) {
    if (!this.tasksByChat.has(chatId)) {
      this.tasksByChat.set(chatId, []);
    }
    const tasks = this.tasksByChat.get(chatId);
    const task = {
      id: randomUUID(),
      chat_id: chatId,
      command,
      cwd: cwd ?? `${PROJECT_ROOT}/batey`,
      state: 'running',
      exit_code: null,
      started_at: startedAt,
      completed_at: null,
      output: initialOutput,
      truncated: false,
    };
    tasks.push(task);
    this.metadataChanged(startedAt);
    return task;
  }

  completeTask(chatId, taskId, exitCode, outputAppend = '') {
    const task = (this.tasksByChat.get(chatId) ?? []).find((t) => t.id === taskId);
    if (!task) return;
    if (outputAppend) task.output += outputAppend;
    task.exit_code = exitCode;
    task.state = exitCode === 0 ? 'completed' : 'failed';
    task.completed_at = now();
    this.metadataChanged();
  }

  activeTurnStartedAt(chatId) {
    let startedAt = null;
    for (const event of this.events
      .filter((candidate) => candidate.session_id === chatId)
      .sort((left, right) => left.seq - right.seq)) {
      if (event.payload.type === 'user_message') startedAt = event.timestamp;
      else if (event.payload.type === 'state_change'
        && event.payload.turn === 'PROMPTING' && startedAt == null) startedAt = event.timestamp;
      else if (event.payload.type === 'turn_complete') startedAt = null;
    }
    return startedAt;
  }

  listChats(projectId) {
    return [...this.chats.values()]
      .filter((chat) => chat.project_id === projectId)
      .sort((a, b) => b.updated_at.localeCompare(a.updated_at) || b.id.localeCompare(a.id))
      .map((chat) => this.chatView(chat));
  }

  touchChatActivity(chatId, updatedAt = now()) {
    const chat = this.chats.get(chatId);
    if (!chat) return null;
    chat.updated_at = updatedAt;
    return chat;
  }

  setRuntime(chatId, process, turn, timestamp = now()) {
    const chat = this.chats.get(chatId);
    if (!chat) return;
    const runtime = this.runtime.get(chatId) ?? { process: 'STOPPED', turn: 'IDLE' };
    const next = { process: process ?? runtime.process, turn: turn ?? runtime.turn };
    this.runtime.set(chatId, next);
    this.emit(chatId, chat.agent, {
      type: 'state_change',
      process: next.process,
      turn: next.turn,
    }, timestamp);
  }

  agent(id) { return AGENTS.find((agent) => agent.id === id); }

  createCustomAgent(input) {
    if (this.agent(input.id)) throw Object.assign(new Error('An agent already uses that id'), { status: 409 });
    const agent = customSummary(input);
    AGENTS.push(agent);
    this.customDetails.set(agent.id, customDetail(input));
    this.metadataChanged();
    return agent;
  }

  editCustomAgent(id, input) {
    const agent = this.agent(id);
    if (!agent) throw Object.assign(new Error('Agent not found'), { status: 404 });
    if (agent.source !== 'batey_managed') throw Object.assign(new Error('This agent is not Batey-managed'), { status: 409 });
    if (input.id !== id) throw Object.assign(new Error('An agent id cannot change'), { status: 400 });
    Object.assign(agent, customSummary(input));
    this.customDetails.set(id, customDetail(input));
    this.metadataChanged();
    return agent;
  }

  agentDetail(id) {
    const agent = this.agent(id);
    if (!agent) throw Object.assign(new Error('Agent not found'), { status: 404 });
    if (agent.source !== 'batey_managed') {
      throw Object.assign(new Error('This agent is not an editable Batey-managed definition'), { status: 409 });
    }
    return { ...this.customDetails.get(id), id };
  }

  removeAgent(id) {
    const index = AGENTS.findIndex((agent) => agent.id === id);
    if (index < 0) throw Object.assign(new Error('Agent not found'), { status: 404 });
    const agent = AGENTS[index];
    if (agent.mutability === 'read_only') throw Object.assign(new Error('This agent is read-only'), { status: 409 });
    AGENTS.splice(index, 1);
    this.customDetails.delete(id);
    this.authByAgent.delete(id);
    this.agentEnv.delete(id);
    this.metadataChanged();
    return { id, deleted: true, retained_chats: 0 };
  }

  agentEnvPresence(id) {
    const agent = this.agent(id);
    if (!agent) throw Object.assign(new Error('Agent not found'), { status: 404 });
    if (agent.mutability !== 'editable' && agent.mutability !== 'registry_managed') {
      throw Object.assign(
        new Error(`Agent '${id}' is not an installed agent. Only Registry-managed and Batey-managed agents take private environment overrides.`),
        { status: 409 },
      );
    }
    const values = this.agentEnv.get(id) ?? new Map();
    return [...values.keys()].sort().map((name) => ({ name, present: true }));
  }

  applyAgentEnvEdits(id, edits) {
    this.agentEnvPresence(id);
    let values = this.agentEnv.get(id);
    if (!values) {
      values = new Map();
      this.agentEnv.set(id, values);
    }
    for (const edit of edits ?? []) {
      const rawName = typeof edit.name === 'string' ? edit.name.trim() : '';
      if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(rawName) || rawName.length > 256) {
        throw Object.assign(
          new Error(`The environment variable '${rawName}' has an unusable name.`),
          { status: 400 },
        );
      }
      const action = edit.action ?? 'replace';
      if (action === 'keep') {
        if (edit.value !== undefined) {
          throw Object.assign(new Error(`The environment variable '${rawName}' is kept, so it takes no value.`), { status: 400 });
        }
        if (!values.has(rawName)) {
          throw Object.assign(new Error(`Cannot keep the unknown environment variable '${rawName}'.`), { status: 400 });
        }
      } else if (action === 'remove') {
        if (edit.value !== undefined) {
          throw Object.assign(new Error(`The environment variable '${rawName}' is removed, so it takes no value.`), { status: 400 });
        }
        values.delete(rawName);
      } else if (action === 'replace') {
        if (typeof edit.value !== 'string') {
          throw Object.assign(new Error(`A replacement value is required for the environment variable '${rawName}'.`), { status: 400 });
        }
        if (edit.value.includes('\0')) {
          throw Object.assign(new Error('An environment value holds no null byte.'), { status: 400 });
        }
        if (!values.has(rawName) && values.size >= 256) {
          throw Object.assign(new Error('An agent takes at most 256 environment overrides.'), { status: 400 });
        }
        values.set(rawName, edit.value);
      } else {
        throw Object.assign(new Error(`Unknown environment action '${action}'.`), { status: 400 });
      }
    }
    return this.agentEnvPresence(id);
  }

  // -------------------------------------------------------- ACP Registry

  /** The browse view. Installed state is derived from the one catalog. */
  registryView(query, forceRefresh = false) {
    const firstFetch = !this.registryFetched;
    const fetch = firstFetch || forceRefresh;
    const error = fetch ? this.registryRefreshError : null;
    if (fetch && !error) {
      this.registryFetched = true;
      this.registryFetchedAt = new Date().toISOString();
    }
    const filter = (query ?? '').trim().toLowerCase();
    const installedByRegistry = new Map(
      AGENTS.filter((agent) => agent.registry_id).map((agent) => [agent.registry_id, agent]),
    );
    const agents = REGISTRY_ENTRIES
      .filter((entry) => !filter || `${entry.id} ${entry.name} ${entry.description}`.toLowerCase().includes(filter))
      .map((entry) => {
        const installed = installedByRegistry.get(entry.id);
        const installedVersion = installed?.display?.version ?? null;
        return {
          ...entry,
          ...(installed ? { installed_as: installed.id, installed_version: installedVersion } : {}),
          update_available: installedVersion !== null && installedVersion !== entry.version,
        };
      });
    return {
      status: !this.registryFetched ? 'unavailable' : fetch && !error ? 'fresh' : 'cached',
      source_url: 'https://registry.example.invalid/registry.json',
      registry_version: '1.0.0',
      ...(this.registryFetchedAt ? { fetched_at: this.registryFetchedAt } : {}),
      ...(error ? { error } : {}),
      host_platform: 'linux-x86_64',
      host: 'linux-x86_64 (fake)',
      rejected: [],
      agents: this.registryFetched ? agents : [],
    };
  }

  installRegistryAgent(body) {
    const entry = REGISTRY_ENTRIES.find((candidate) => candidate.id === body.registry_id);
    if (!entry) throw Object.assign(new Error('Registry agent not found'), { status: 404 });
    if (entry.unsupported_reason) {
      throw Object.assign(new Error(entry.unsupported_reason), { status: 422 });
    }
    const id = (body.agent_id ?? '').trim() || entry.id;
    if (this.agent(id)) throw Object.assign(new Error('An agent already uses that id'), { status: 409 });
    const agent = {
      id,
      display_name: body.display_name?.trim() || entry.name,
      source: 'registry',
      registry_id: entry.id,
      availability: 'available',
      usage_provider: body.usage_provider ?? null,
      metadata: body.metadata ?? null,
      mutability: 'registry_managed',
      display: { description: entry.description, version: entry.version },
    };
    AGENTS.push(agent);
    this.metadataChanged();
    return agent;
  }

  updateRegistryAgent(id) {
    const agent = this.agent(id);
    if (!agent) throw Object.assign(new Error('Agent not found'), { status: 404 });
    if (agent.source !== 'registry') throw Object.assign(new Error('Only registry agents can update'), { status: 409 });
    const entry = REGISTRY_ENTRIES.find((candidate) => candidate.id === agent.registry_id);
    if (!entry) throw Object.assign(new Error('The registry entry is gone'), { status: 404 });
    const from = agent.display.version ?? '0.0.0';
    if (from === entry.version) {
      return { updated: false, from_version: from, to_version: entry.version, agent };
    }
    agent.display = { ...agent.display, version: entry.version };
    this.metadataChanged();
    return { updated: true, from_version: from, to_version: entry.version, agent };
  }

  // ------------------------------------------------------- authentication

  observedAuth(id) {
    return this.observedByAgent?.get(id) ?? 'unknown';
  }

  setObservedAuth(id, state) {
    if (!this.observedByAgent) this.observedByAgent = new Map();
    this.observedByAgent.set(id, state);
  }

  agentAuth(id) {
    if (!this.agent(id)) throw Object.assign(new Error('Agent not found'), { status: 404 });
    const config = AUTH_METHODS[id] ?? { logout_supported: false, methods: [] };
    return {
      agent_id: id,
      methods: config.methods.map((method) => ({ ...method })),
      logout_supported: config.logout_supported,
      terminal_supported: true,
      observed_state: this.observedAuth(id),
      active_flow: this.activeFlowFor(id),
    };
  }

  /**
   * Safe active-flow discovery for one agent. Contains only flow id, kind,
   * method id, lifecycle state, and start time. Never PTY output, codes,
   * tokens, or sensitive URLs.
   */
  activeFlowFor(id) {
    const candidates = [];
    for (const flow of this.protocolFlows.values()) {
      if (flow.agent_id !== id) continue;
      if (flow.state !== 'running' && flow.state !== 'waiting_for_user') continue;
      candidates.push({
        flow_id: flow.flow_id,
        kind: 'protocol',
        method_id: flow.method_id,
        state: flow.state,
        started_at: flow.started_at,
      });
    }
    for (const flow of this.flows.values()) {
      if (flow.agent_id !== id || flow.state !== 'running') continue;
      candidates.push({
        flow_id: flow.flow_id,
        kind: 'terminal',
        method_id: flow.method_id,
        state: flow.state,
        started_at: flow.started_at,
      });
    }
    if (candidates.length === 0) return null;
    candidates.sort((a, b) => (b.started_at ?? '').localeCompare(a.started_at ?? ''));
    return candidates[0];
  }

  authenticateAgent(id, methodId) {
    const auth = this.agentAuth(id);
    const method = auth.methods.find((candidate) => candidate.id === methodId);
    if (!method) throw Object.assign(new Error(`Unknown authentication method '${methodId}'`), { status: 404 });
    if (method.type === 'terminal') {
      throw Object.assign(new Error(`Authentication method '${methodId}' runs in a terminal. Start a terminal authentication flow instead.`), { status: 400 });
    }
    if (method.type !== 'agent') {
      throw Object.assign(new Error(`Authentication method '${methodId}' uses the unsupported type '${method.type}'`), { status: 400 });
    }
    this.setObservedAuth(id, 'authenticated');
    return this.agentAuth(id);
  }

  logoutAgent(id) {
    const auth = this.agentAuth(id);
    if (!auth.logout_supported) {
      throw Object.assign(new Error(`Agent '${id}' does not support logout`), { status: 409 });
    }
    this.setObservedAuth(id, 'authentication_required');
    return this.agentAuth(id);
  }

  startTerminalFlow(id, methodId) {
    const auth = this.agentAuth(id);
    const method = auth.methods.find((candidate) => candidate.id === methodId);
    if (!method) throw Object.assign(new Error(`Unknown authentication method '${methodId}'`), { status: 404 });
    if (method.type !== 'terminal') {
      throw Object.assign(new Error(`Authentication method '${methodId}' is not a terminal method`), { status: 400 });
    }
    const flow = {
      flow_id: randomUUID(),
      agent_id: id,
      method_id: methodId,
      state: 'running',
      exit_code: null,
      reason: null,
      started_at: new Date().toISOString(),
      completed_at: null,
      output: `Sign in to ${id}.\nType a token and press Enter. Type "fail" to simulate a failure.\n`,
      cols: 80,
      rows: 24,
      socket: null,
    };
    this.flows.set(flow.flow_id, flow);
    return flow;
  }

  flowView(flowId) {
    const flow = this.flows.get(flowId);
    if (!flow) return null;
    const { socket, output, cols, rows, ...view } = flow;
    return view;
  }

  attachFlowSocket(flowId, socket) {
    const flow = this.flows.get(flowId);
    if (!flow) {
      socket.close();
      return;
    }
    flow.socket = socket;
    socket.send(JSON.stringify({ type: 'output', data: flow.output }));
    socket.send(JSON.stringify({
      type: 'state',
      flow_id: flow.flow_id,
      agent_id: flow.agent_id,
      method_id: flow.method_id,
      state: flow.state,
      exit_code: flow.exit_code,
      reason: flow.reason,
    }));
    socket.onMessage = (text) => {
      let message;
      try {
        message = JSON.parse(text);
      } catch {
        return;
      }
      if (message.type === 'input') this.flowInput(flowId, String(message.data ?? ''));
      else if (message.type === 'resize') this.flowResize(flowId, message.cols, message.rows);
    };
    socket.onClose = () => {
      if (flow.socket === socket) flow.socket = null;
    };
  }

  flowInput(flowId, data) {
    const flow = this.flows.get(flowId);
    if (!flow || flow.state !== 'running') return;
    flow.output += data;
    this.sendFlow(flow, { type: 'output', data });
    if (data.includes('\r') || data.includes('\n')) {
      const line = flow.output.split('\n').pop().replace(/\r/g, '').trim().toLowerCase();
      if (line.includes('fail')) this.finishFlow(flowId, 'failed', 1, 'The authentication command failed.');
      else if (line.includes('cancel')) this.finishFlow(flowId, 'cancelled', null, 'Cancelled by the client');
      else if (line.includes('timeout')) this.finishFlow(flowId, 'timed_out', null, 'The authentication flow timed out.');
      else this.finishFlow(flowId, 'succeeded', 0, null);
    }
  }

  flowResize(flowId, cols, rows) {
    const flow = this.flows.get(flowId);
    if (!flow) return;
    flow.cols = Number(cols) || flow.cols;
    flow.rows = Number(rows) || flow.rows;
  }

  cancelFlow(flowId) {
    const flow = this.flows.get(flowId);
    if (!flow) throw Object.assign(new Error('Authentication flow not found'), { status: 404 });
    if (flow.state === 'running') this.finishFlow(flowId, 'cancelled', null, 'Cancelled by the client');
    return this.flowView(flowId);
  }

  finishFlow(flowId, flowState, exitCode, reason) {
    const flow = this.flows.get(flowId);
    if (!flow || flow.state !== 'running') return;
    flow.state = flowState;
    flow.exit_code = exitCode;
    flow.reason = reason;
    flow.completed_at = new Date().toISOString();
    if (flowState === 'succeeded') this.setObservedAuth(flow.agent_id, 'authenticated');
    this.sendFlow(flow, {
      type: 'state',
      flow_id: flow.flow_id,
      agent_id: flow.agent_id,
      method_id: flow.method_id,
      state: flowState,
      exit_code: exitCode,
      reason: reason,
    });
  }

  sendFlow(flow, message) {
    if (flow.socket?.open) flow.socket.send(JSON.stringify(message));
  }

  // ------------------------------------------ async protocol auth flows

  startProtocolFlow(id, methodId) {
    const auth = this.agentAuth(id);
    const method = auth.methods.find((candidate) => candidate.id === methodId);
    if (!method) throw Object.assign(new Error(`Unknown authentication method '${methodId}'`), { status: 404 });
    if (method.type === 'terminal') {
      throw Object.assign(new Error(`Authentication method '${methodId}' runs in a terminal. Start a terminal authentication flow instead.`), { status: 400 });
    }
    if (method.type !== 'agent') {
      throw Object.assign(new Error(`Authentication method '${methodId}' uses the unsupported type '${method.type}'`), { status: 400 });
    }
    const active = [...this.protocolFlows.values()].filter(
      (flow) => flow.agent_id === id && (flow.state === 'running' || flow.state === 'waiting_for_user'),
    );
    if (active.length >= 1) {
      throw Object.assign(new Error(`Agent '${id}' already has an authentication flow running. Finish or cancel it first.`), { status: 409 });
    }
    const flow = {
      flow_id: randomUUID().replace(/-/g, '') + randomUUID().replace(/-/g, ''),
      agent_id: id,
      method_id: methodId,
      state: 'running',
      reason: null,
      started_at: new Date().toISOString(),
      completed_at: null,
    };
    this.protocolFlows.set(flow.flow_id, flow);
    // Request-scoped elicitations, never durable chat events. Codex uses a
    // URL device-code step; Antigravity uses an interactive form step.
    if (id === 'codex' && methodId === 'openai-oauth') {
      this.protocolElicitations.set(flow.flow_id, [
        {
          id: `${flow.flow_id}:device`,
          mode: 'url',
          message: 'Open the device page and enter the code.',
          schema: null,
          url: 'https://example.invalid/device?code=ABCD-1234',
          elicitation_id: `${flow.flow_id}:device`,
          tool_call_id: null,
        },
      ]);
      flow.state = 'waiting_for_user';
    } else if (id === 'antigravity') {
      this.protocolElicitations.set(flow.flow_id, [
        {
          id: `${flow.flow_id}:interactive`,
          mode: 'form',
          message: 'Complete the interactive sign-in step.',
          schema: { properties: {}, required: [] },
          url: null,
          elicitation_id: null,
          tool_call_id: null,
        },
      ]);
      flow.state = 'waiting_for_user';
    } else {
      this.protocolElicitations.set(flow.flow_id, []);
      // A plain agent method with no elicitation succeeds at once in the
      // fake, so polling never sticks in running.
      flow.state = 'succeeded';
      flow.completed_at = new Date().toISOString();
      this.setObservedAuth(id, 'authenticated');
    }
    return { ...flow };
  }

  protocolFlowView(flowId) {
    const flow = this.protocolFlows.get(flowId);
    if (!flow) return null;
    return { ...flow };
  }

  cancelProtocolFlow(flowId) {
    const flow = this.protocolFlows.get(flowId);
    if (!flow) throw Object.assign(new Error('Authentication flow not found'), { status: 404 });
    if (flow.state === 'running' || flow.state === 'waiting_for_user') {
      flow.state = 'cancelled';
      flow.reason = 'Cancelled by the client';
      flow.completed_at = new Date().toISOString();
      this.protocolElicitations.delete(flowId);
    }
    return { ...flow };
  }

  listProtocolElicitations(flowId) {
    if (!this.protocolFlows.get(flowId)) {
      throw Object.assign(new Error('Authentication flow not found'), { status: 404 });
    }
    return (this.protocolElicitations.get(flowId) ?? []).map((entry) => ({ ...entry }));
  }

  respondProtocolElicitation(flowId, eid, action, content) {
    const flow = this.protocolFlows.get(flowId);
    if (!flow) throw Object.assign(new Error('Authentication flow not found'), { status: 404 });
    const list = this.protocolElicitations.get(flowId) ?? [];
    const index = list.findIndex((entry) => entry.id === eid);
    if (index < 0) throw Object.assign(new Error('Elicitation not found'), { status: 404 });
    if (!['accept', 'decline', 'cancel'].includes(action)) {
      throw Object.assign(new Error('Elicitation action must be accept, decline, or cancel'), { status: 400 });
    }
    list.splice(index, 1);
    this.protocolElicitations.set(flowId, list);
    if (action === 'accept') {
      if (list.length === 0) {
        flow.state = 'succeeded';
        flow.reason = null;
        flow.completed_at = new Date().toISOString();
        this.setObservedAuth(flow.agent_id, 'authenticated');
      } else {
        flow.state = 'waiting_for_user';
      }
    } else if (action === 'decline') {
      flow.state = 'failed';
      flow.reason = 'The authentication step was declined.';
      flow.completed_at = new Date().toISOString();
    } else {
      flow.state = 'cancelled';
      flow.reason = 'Cancelled by the client';
      flow.completed_at = new Date().toISOString();
    }
    return true;
  }

  // ------------------------------------------------------------------ seed

  /** Seeds the editable definition and the representative auth states. */
  seedAgents() {
    const custom = {
      id: 'my-custom',
      display_name: 'My Custom Agent',
      command: 'my-agent',
      args: ['--acp'],
      env: { MY_AGENT_TOKEN: 'fake-token' },
      idle_timeout: 900,
      usage_provider: null,
      metadata: null,
      default_permission_policy: 'ask',
      description: 'A Batey-managed custom agent.',
    };
    if (!this.agent(custom.id)) AGENTS.push(customSummary(custom));
    this.customDetails.set(custom.id, customDetail(custom));
  }

  seed() {
    this.seedAgents();
    const hub = this.createProject('batey', `${PROJECT_ROOT}/batey`, {
      is_git: true,
      current_branch: 'master',
      head_sha: '1111111111111111111111111111111111111111',
      dirty: true,
      branches: [
        { name: 'master', sha: '1111111111111111111111111111111111111111', current: true },
        { name: 'feature/ui', sha: '2222222222222222222222222222222222222222', current: false },
        { name: 'release', sha: '3333333333333333333333333333333333333333', current: false },
      ],
    });
    const firmware = this.createProject('corolla-firmware', `${PROJECT_ROOT}/corolla-firmware`);
    const scratch = this.createProject('scratch', `${PROJECT_ROOT}/scratch`);
    this.setSeedProjectDates(hub, '2026-09-15T08:00:00.000Z', '2026-09-15T15:40:00.000Z');
    this.setSeedProjectDates(firmware, '2026-09-12T09:00:00.000Z', '2026-09-15T14:10:00.000Z');
    this.setSeedProjectDates(scratch, '2026-09-10T10:00:00.000Z', '2026-09-15T13:10:00.000Z');

    // The titles and timestamps are deliberately stable so a fresh `npm run
    // dev` presents the same useful starting point every time. UUIDs remain
    // realistic because managed workspace branches contain chat UUIDs.
    const review = this.createChat(hub.id, 'claude', 'Review the WebSocket replay path');
    this.setManagedWorkspace(review, '1111111111111111111111111111111111111111');
    review.acp_session_id = 'acp-session-review-replay';
    this.seedTranscript(review, '2026-09-15T15:40:00.000Z');
    this.setSeedUsage(review, { used: 18400, size: 200000, cost_amount: 0.184, cost_currency: 'USD' });

    const working = this.createChat(hub.id, 'codex', 'Trace the reconnect race in EventLog');
    this.setManagedWorkspace(working, '2222222222222222222222222222222222222222');
    working.acp_session_id = 'acp-session-reconnect-race';
    this.seedIncompleteTurn(working, '2026-09-15T15:35:00.000Z');
    this.setSeedConfig(working, 'model', 'gpt-5');
    this.setSeedUsage(working, { used: 5200, size: 200000, cost_amount: 0.052, cost_currency: 'USD' });

    const waiting = this.createChat(hub.id, 'opencode', 'Approve the WebSocket backpressure fix');
    this.setProjectCheckout(waiting, 'feature/ui', '2222222222222222222222222222222222222222');
    waiting.acp_session_id = 'acp-session-backpressure';
    this.seedPermissionTurn(waiting, '2026-09-15T15:30:00.000Z', 'seed-permission-backpressure');
    this.setSeedUsage(waiting, { used: 7600, size: 200000, cost_amount: 0.076, cost_currency: 'USD' });

    const failed = this.createChat(hub.id, 'claude', 'Recover the failed schema migration check');
    this.setProjectCheckout(failed, 'feature/ui', '2222222222222222222222222222222222222222');
    failed.acp_session_id = 'acp-session-migration-failure';
    this.seedFailedTurn(failed, '2026-09-15T15:25:00.000Z');
    this.setSeedConfig(failed, 'reasoning_effort', 'high');
    this.setSeedUsage(failed, { used: 31200, size: 200000, cost_amount: 0.312, cost_currency: 'USD' });

    const terminal = this.createChat(hub.id, 'antigravity', 'Run the workspace verification suite');
    this.setManagedWorkspace(terminal, '1111111111111111111111111111111111111111');
    terminal.acp_session_id = 'acp-session-verification-task';
    this.seedConversation(terminal, {
      user: 'Can you run the workspace verification suite and leave the output available?',
      thought: 'The suite is long enough to keep as a background terminal task while I report the launch details.',
      answer: 'I started the verification suite in a background terminal task. Open Terminal tasks to follow its output.',
    }, '2026-09-15T15:20:00.000Z');
    this.createTask(
      terminal.id,
      'nix run .#verify',
      `${PROJECT_ROOT}/batey`,
      'checking Rust formatting…\nwaiting for frontend build…',
      '2026-09-15T15:20:04.000Z',
    );
    this.setSeedUsage(terminal, { used: 9400, size: 200000, cost_amount: 0.094, cost_currency: 'USD' });

    const blocked = this.createChat(hub.id, 'codex', 'Authorize the project .envrc before running tests');
    this.setProjectCheckout(blocked, 'feature/ui', '2222222222222222222222222222222222222222');
    blocked.acp_session_id = 'acp-session-blocked-env';
    this.seedConversation(blocked, {
      user: 'Why does the test runner need the project environment?',
      thought: 'The workspace uses direnv to provide the pinned toolchain and test credentials.',
      answer: 'The environment is ready once the project .envrc is authorized. The next connection attempt will retry the agent.',
    }, '2026-09-15T15:15:00.000Z');
    this.blockEnvironment(blocked.id);
    this.setSeedUsage(blocked, { used: 2800, size: 200000, cost_amount: 0.028, cost_currency: 'USD' });

    const archived = this.createChat(hub.id, 'codex', 'Port the store to versioned migrations');
    this.setManagedWorkspace(archived, '1111111111111111111111111111111111111111');
    archived.acp_session_id = 'acp-session-archived-migrations';
    archived.archived = true;
    this.seedConversation(archived, {
      user: 'How do we port the store to versioned migrations?',
      thought: 'The store opens SQLite directly. I must list the tables before I draft the migration steps.',
      answer: 'I drafted the migration plan. Each migration runs once and records its version, so a restart never replays it.',
    }, '2026-09-15T14:40:00.000Z');
    this.setSeedUsage(archived, { used: 6600, size: 200000, cost_amount: 0.066, cost_currency: 'USD' });

    const firmwareReview = this.createChat(firmware.id, 'claude', 'Inspect the Corolla calibration checksum');
    firmwareReview.acp_session_id = 'acp-session-calibration';
    this.seedConversation(firmwareReview, {
      user: 'Can you inspect the calibration checksum without changing the dump?',
      thought: 'This is a read-only review. I will compare the checksum routine with the captured bytes.',
      answer: 'The checksum mismatch is isolated to the final calibration block; no files were changed under the read-only policy.',
    }, '2026-09-15T14:10:00.000Z');
    this.setSeedUsage(firmwareReview, { used: 4100, size: 200000, cost_amount: 0.041, cost_currency: 'USD' });

    const rich = this.createChat(scratch.id, 'opencode', 'Compare the dashboard render trace and screenshot');
    rich.acp_session_id = 'acp-session-rich-render';
    this.seedRichConversation(rich, '2026-09-15T13:10:00.000Z');
    this.setSeedConfig(rich, 'model', 'claude-sonnet-5');
    this.setSeedUsage(rich, { used: 14300, size: 200000, cost_amount: 0.143, cost_currency: 'USD' });

    // A genuinely new chat exercises the empty-state composer and connection
    // setup without requiring a prompt or a special frontend branch.
    const fresh = this.createChat(scratch.id, 'example-acp', 'Draft a release checklist');
    fresh.acp_session_id = null;
    this.setSeedChatDates(fresh, '2026-09-15T12:50:00.000Z', '2026-09-15T12:50:00.000Z');
    this.setSeedConfig(fresh, 'web_search', true);
    this.setSeedUsage(fresh, { used: 0, size: 200000, cost_amount: 0, cost_currency: 'USD' });

    this.seedAuthFlows();
  }

  /**
   * Seeds one recoverable protocol flow and one recoverable terminal flow, so
   * the Agents page exercises automatic rediscovery after navigation/reload.
   * The token/URL values are fictional fixtures, never real credentials.
   */
  seedAuthFlows() {
    // Defensive: the shared AGENTS fixture is mutable across tests, so seed
    // only when the agent is still present.
    if (this.agent('antigravity')) {
      this.startProtocolFlow('antigravity', 'antigravity-interactive');
    }
    if (this.agent('example-acp')) {
      this.startTerminalFlow('example-acp', 'example-token');
    }
  }

  /**
   * Writes one finished user/answer turn from a small script, so each seeded
   * chat shows history without duplicating `emit()` blocks.
   */
  seedConversation(chat, script, startAt) {
    this.setSeedCreatedAt(chat, startAt);
    const timestamp = (seconds) => new Date(Date.parse(startAt) + seconds * 1000).toISOString();
    const userEvent = this.emit(chat.id, chat.agent, {
      type: 'user_message',
      text: script.user,
      ...(script.userContent ? { content: script.userContent } : {}),
    }, timestamp(0));
    this.touchChatActivity(chat.id, userEvent.timestamp);
    if (script.thought) {
      this.emit(chat.id, chat.agent, {
        type: 'thought_chunk',
        text: script.thought,
        ...(script.thoughtContent ? { content: script.thoughtContent } : {}),
      }, timestamp(1));
    }
    this.emit(chat.id, chat.agent, {
      type: 'message_chunk',
      text: script.answer,
      ...(script.answerContent ? { content: script.answerContent } : {}),
    }, timestamp(2));
    this.emit(chat.id, chat.agent, { type: 'turn_complete', stop_reason: 'end_turn' }, timestamp(3));
  }

  /** Writes a finished turn, so a freshly opened UI already shows content. */
  seedTranscript(chat, startAt) {
    this.setSeedCreatedAt(chat, startAt);
    const agent = chat.agent;
    const timestamp = (seconds) => new Date(Date.parse(startAt) + seconds * 1000).toISOString();
    const userEvent = this.emit(chat.id, agent, {
      type: 'user_message',
      text: 'Why does the WebSocket drop events after a reconnect?',
    }, timestamp(0));
    this.touchChatActivity(chat.id, userEvent.timestamp);
    this.emit(chat.id, agent, {
      type: 'thought_chunk',
      text: 'The client sends from_seq. I must check how the log replays it.',
    }, timestamp(1));
    this.emit(chat.id, agent, {
      type: 'tool_call',
      id: 'seed-tool-1',
      title: 'Read backend/src/events.rs',
      kind: 'read',
      status: 'in_progress',
    }, timestamp(2));
    this.emit(chat.id, agent, {
      type: 'tool_call_update',
      id: 'seed-tool-1',
      status: 'completed',
      output: 'replay_from() filters on seq >= from_seq.',
    }, timestamp(3));
    this.emit(chat.id, agent, {
      type: 'message_chunk',
      text: 'The replay is correct. ',
    }, timestamp(4));
    this.emit(chat.id, agent, {
      type: 'message_chunk',
      text: 'The gap comes from the broadcast channel, which drops a slow reader.',
    }, timestamp(5));
    this.emit(chat.id, agent, { type: 'turn_complete', stop_reason: 'end_turn' }, timestamp(6));
  }

  /** Writes an open turn with partial ACP output and no completion event. */
  seedIncompleteTurn(chat, startAt) {
    this.setSeedCreatedAt(chat, startAt);
    const timestamp = (seconds) => new Date(Date.parse(startAt) + seconds * 1000).toISOString();
    const userEvent = this.emit(chat.id, chat.agent, {
      type: 'user_message',
      text: 'Trace the reconnect race and show me where the event high-water mark moves.',
    }, timestamp(0));
    this.touchChatActivity(chat.id, userEvent.timestamp);
    this.setRuntime(chat.id, 'RUNNING', 'PROMPTING', timestamp(1));
    this.emit(chat.id, chat.agent, {
      type: 'thought_chunk',
      text: 'I am comparing the replay cursor with the broadcast subscriber now.',
    }, timestamp(2));
    this.emit(chat.id, chat.agent, {
      type: 'plan',
      entries: [
        { content: 'Compare replay and live delivery', status: 'completed' },
        { content: 'Trace the reconnect cursor update', status: 'in_progress' },
        { content: 'Write a regression test', status: 'pending' },
      ],
    }, timestamp(3));
    this.emit(chat.id, chat.agent, {
      type: 'tool_call',
      id: 'seed-working-tool',
      title: 'Read backend/src/events.rs::replay_page',
      kind: 'read',
      status: 'in_progress',
      locations: [{ path: 'backend/src/events.rs', line: 342 }],
    }, timestamp(4));
    this.emit(chat.id, chat.agent, {
      type: 'message_chunk',
      text: 'The replay cursor is still open while I inspect the subscriber handoff…',
    }, timestamp(5));
    this.seededTurns.set(chat.id, { kind: 'working' });
  }

  /** Writes an open turn whose permission request is still unresolved. */
  seedPermissionTurn(chat, startAt, permissionId) {
    this.setSeedCreatedAt(chat, startAt);
    const timestamp = (seconds) => new Date(Date.parse(startAt) + seconds * 1000).toISOString();
    const userEvent = this.emit(chat.id, chat.agent, {
      type: 'user_message',
      text: 'Apply the backpressure fix to the WebSocket send loop.',
    }, timestamp(0));
    this.touchChatActivity(chat.id, userEvent.timestamp);
    this.setRuntime(chat.id, 'RUNNING', 'PROMPTING', timestamp(1));
    this.emit(chat.id, chat.agent, {
      type: 'thought_chunk',
      text: 'The fix changes the sender loop, so I need approval before writing the Rust handler.',
    }, timestamp(2));
    this.emit(chat.id, chat.agent, {
      type: 'tool_call',
      id: 'seed-waiting-tool',
      title: 'Edit backend/src/web/websocket.rs',
      kind: 'edit',
      status: 'in_progress',
    }, timestamp(3));
    this.emit(chat.id, chat.agent, {
      type: 'permission_request',
      id: permissionId,
      method: 'fs/write_text_file',
      title: 'Write WebSocket sender loop',
      kind: 'edit',
      description: 'Write backend/src/web/websocket.rs to preserve the reconnect high-water mark.',
      options: [
        { optionId: 'seed-allow-once', name: 'Allow once', kind: 'allow_once' },
        { optionId: 'seed-reject-once', name: 'Reject once', kind: 'reject_once' },
      ],
    }, timestamp(4));
    this.seededTurns.set(chat.id, { kind: 'waiting', permission_id: permissionId });
  }

  /** Writes both an ACP error event and the failed stop reason. */
  seedFailedTurn(chat, startAt) {
    this.setSeedCreatedAt(chat, startAt);
    const timestamp = (seconds) => new Date(Date.parse(startAt) + seconds * 1000).toISOString();
    const userEvent = this.emit(chat.id, chat.agent, {
      type: 'user_message',
      text: 'Run the migration check and explain any schema failure.',
    }, timestamp(0));
    this.touchChatActivity(chat.id, userEvent.timestamp);
    this.setRuntime(chat.id, 'RUNNING', 'PROMPTING', timestamp(1));
    this.emit(chat.id, chat.agent, {
      type: 'thought_chunk',
      text: 'I am opening the migration check output before reporting the failure.',
    }, timestamp(2));
    this.emit(chat.id, chat.agent, {
      type: 'error',
      message: 'Agent process exited with code 1: migration check found a duplicate user_version advance.',
    }, timestamp(3));
    this.emit(chat.id, chat.agent, { type: 'turn_complete', stop_reason: 'error' }, timestamp(4));
    this.setRuntime(chat.id, 'DEAD', 'IDLE', timestamp(5));
  }

  seedRichConversation(chat, startAt) {
    this.setSeedCreatedAt(chat, startAt);
    const timestamp = (seconds) => new Date(Date.parse(startAt) + seconds * 1000).toISOString();
    const content = RICH_HISTORY_CONTENT.map((block) => ({ ...block }));
    const userEvent = this.emit(chat.id, chat.agent, {
      type: 'user_message',
      text: 'Compare the dashboard screenshot with the render trace and identify the layout shift.',
      content: content.slice(0, 2),
    }, timestamp(0));
    this.touchChatActivity(chat.id, userEvent.timestamp);
    this.emit(chat.id, chat.agent, {
      type: 'thought_chunk',
      text: 'The screenshot and the trace agree on a small first-paint shift; I will correlate the route and resource metadata.',
    }, timestamp(1));
    this.emit(chat.id, chat.agent, {
      type: 'tool_call',
      id: 'seed-rich-tool',
      title: 'Inspect dashboard render trace',
      kind: 'read',
      status: 'in_progress',
      content: [{ type: 'content', content: { type: 'text', text: 'Reading the retained render trace metadata.' } }],
    }, timestamp(2));
    this.emit(chat.id, chat.agent, {
      type: 'tool_call_update',
      id: 'seed-rich-tool',
      status: 'completed',
      output: 'firstPaintMs=184; layoutShift=0.01',
      content: [{ type: 'content', content: content[3] }],
    }, timestamp(3));
    this.emit(chat.id, chat.agent, {
      type: 'message_chunk',
      text: 'The screenshot matches the captured route. The small layout shift comes from the deferred status badge, not the project content.',
      content: content.slice(2),
    }, timestamp(4));
    this.emit(chat.id, chat.agent, { type: 'turn_complete', stop_reason: 'end_turn' }, timestamp(5));
  }

  setManagedWorkspace(chat, baseCommit) {
    chat.workspace = {
      mode: 'managed_worktree',
      branch: `batey/chat/${chat.id}`,
      base_commit: baseCommit,
    };
  }

  setProjectCheckout(chat, branch, baseCommit) {
    chat.workspace = { mode: 'project_checkout', branch, base_commit: baseCommit };
  }

  setSeedProjectDates(project, createdAt, updatedAt) {
    project.created_at = createdAt;
    project.updated_at = updatedAt;
  }

  setSeedChatDates(chat, createdAt, updatedAt) {
    chat.created_at = createdAt;
    chat.updated_at = updatedAt;
  }

  setSeedCreatedAt(chat, startAt) {
    chat.created_at = new Date(Date.parse(startAt) - 60 * 60 * 1000).toISOString();
  }

  setSeedUsage(chat, usage) {
    this.usageByChat.set(chat.id, usage);
  }

  setSeedConfig(chat, optionId, value) {
    const options = this.configByChat.get(chat.id) ?? [];
    const option = options.find((candidate) => candidate.id === optionId);
    if (option) option.currentValue = value;
    chat.config_values = { ...chat.config_values, [optionId]: value };
  }
}

function customSummary(input) {
  validateCustomInput(input);
  return {
    id: input.id.trim(), display_name: input.display_name?.trim() || input.id.trim(),
    source: 'batey_managed', availability: 'available', usage_provider: input.usage_provider ?? null,
    metadata: input.metadata ?? null, mutability: 'editable',
    display: {
      ...(input.description?.trim() ? { description: input.description.trim() } : {}),
      ...(input.display?.version ? { version: input.display.version } : {}),
    },
  };
}

/** The authenticated per-agent management detail, including launch env. */
function customDetail(input) {
  return {
    id: input.id.trim(),
    display_name: input.display_name?.trim() || input.id.trim(),
    command: input.command ?? '',
    args: Array.isArray(input.args) ? [...input.args] : [],
    env: { ...(input.env ?? {}) },
    idle_timeout: input.idle_timeout ?? 900,
    usage_provider: input.usage_provider ?? null,
    metadata: input.metadata ?? null,
    default_permission_policy: input.default_permission_policy ?? 'ask',
    description: input.description?.trim() || null,
  };
}

export function validateCustomInput(input) {
  const issues = [];
  if (!input || typeof input.id !== 'string' || !/^[A-Za-z0-9_-]+$/.test(input.id.trim())) issues.push({ field: 'id', message: "An agent id holds letters, digits, '-', and '_' only." });
  if (!input || typeof input.command !== 'string' || !input.command.trim()) issues.push({ field: 'command', message: 'An agent needs a command.' });
  return { valid: issues.length === 0, issues };
}
