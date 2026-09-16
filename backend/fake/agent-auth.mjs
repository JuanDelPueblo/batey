// Agent-level authentication for the fake backend.
//
// It mirrors the REST and WebSocket contract of `backend/src/web/agent_auth.rs`
// so the Agents management surface runs without a Rust build and without an
// agent binary. It starts no process: a terminal flow is a scripted echo.
import { randomBytes } from 'node:crypto';

/** What each fake agent advertises at `initialize`. */
const AUTH_METHODS = {
  claude: {
    logout_supported: true,
    methods: [
      { id: 'claude-login', name: 'Log in with Claude', description: 'Open a browser login.', type: 'agent', supported: true },
      { id: 'claude-terminal', name: 'Log in in a terminal', description: 'Run the agent in an interactive terminal.', type: 'terminal', supported: true },
    ],
  },
  codex: {
    logout_supported: true,
    methods: [
      { id: 'codex-terminal', name: 'Log in in a terminal', description: null, type: 'terminal', supported: true },
      { id: 'codex-future', name: 'Future scheme', description: 'A method type this build cannot run.', type: 'browser-popup', supported: false },
    ],
  },
  opencode: {
    logout_supported: false,
    methods: [{ id: 'api-key', name: 'API key', description: 'Paste an API key.', type: 'agent', supported: true }],
  },
  antigravity: { logout_supported: false, methods: [] },
};

const MAX_ACTIVE_FLOWS_PER_AGENT = 1;
const MAX_SCROLLBACK_BYTES = 64 * 1024;

export class FakeAgentAuth {
  constructor() {
    /** @type {Map<string, object>} */
    this.flows = new Map();
    this.authenticated = new Set();
    this.observed = new Map();
  }

  observedState(agentId) {
    return this.observed.get(agentId) ?? 'unknown';
  }

  agentView(agentId) {
    const state = AUTH_METHODS[agentId];
    if (!state) return null;
    return {
      agent_id: agentId,
      methods: state.methods.map((method) => ({ ...method })),
      logout_supported: state.logout_supported,
      terminal_supported: true,
      observed_state: this.observedState(agentId),
      active_flow: this.activeFlowFor(agentId),
    };
  }

  /** Safe active-flow discovery: lifecycle only, never PTY material. */
  activeFlowFor(agentId) {
    const flow = [...this.flows.values()]
      .filter((candidate) => candidate.agent_id === agentId && candidate.state === 'running')
      .sort((a, b) => (b.started_at ?? '').localeCompare(a.started_at ?? ''))[0];
    if (!flow) return null;
    return {
      flow_id: flow.flow_id,
      kind: 'terminal',
      method_id: flow.method_id,
      state: flow.state,
      started_at: flow.started_at,
    };
  }

  method(agentId, methodId) {
    return AUTH_METHODS[agentId]?.methods.find((method) => method.id === methodId) ?? null;
  }

  startFlow(agentId, methodId) {
    const active = [...this.flows.values()].filter(
      (flow) => flow.agent_id === agentId && flow.state === 'running',
    );
    if (active.length >= MAX_ACTIVE_FLOWS_PER_AGENT) {
      return { error: `Agent '${agentId}' already has an authentication flow running.` };
    }
    const flow = {
      // Opaque, like the real backend's identifiers.
      flow_id: randomBytes(32).toString('hex'),
      agent_id: agentId,
      method_id: methodId,
      state: 'running',
      exit_code: null,
      reason: null,
      started_at: new Date().toISOString(),
      completed_at: null,
      scrollback: `Fake terminal for ${agentId}.\r\nType "ok" to finish, "fail" to fail.\r\n`,
      listeners: new Set(),
    };
    this.flows.set(flow.flow_id, flow);
    return { flow };
  }

  get(flowId) {
    return this.flows.get(flowId) ?? null;
  }

  flowView(flow) {
    const { scrollback: _scrollback, listeners: _listeners, ...view } = flow;
    return view;
  }

  /** Writes terminal output to the bounded scrollback and to every client. */
  emit(flow, text) {
    flow.scrollback = (flow.scrollback + text).slice(-MAX_SCROLLBACK_BYTES);
    for (const listener of flow.listeners) listener({ type: 'output', data: text });
  }

  finish(flow, state, exitCode, reason) {
    if (flow.state !== 'running') return;
    flow.state = state;
    flow.exit_code = exitCode;
    flow.reason = reason;
    flow.completed_at = new Date().toISOString();
    if (state === 'succeeded') {
      this.authenticated.add(flow.agent_id);
      this.observed.set(flow.agent_id, 'authenticated');
    }
    for (const listener of flow.listeners) {
      listener({ type: 'state', ...this.flowView(flow) });
    }
  }

  /** Handles one client message on a flow socket. */
  handleMessage(flow, message) {
    if (message.type === 'resize') return;
    if (message.type !== 'input') return;
    const data = String(message.data ?? '');
    // A real terminal echoes what the user typed.
    this.emit(flow, data.replace(/\n/g, '\r\n'));
    for (const line of data.split('\n')) {
      const command = line.trim();
      if (command === 'ok') {
        this.finish(flow, 'succeeded', 0, null);
        return;
      }
      if (command === 'fail') {
        this.finish(flow, 'failed', 3, 'The authentication command exited with status 3');
        return;
      }
      if (command) this.emit(flow, `echo:${command}\r\n`);
    }
  }

  cancel(flow) {
    this.finish(flow, 'cancelled', null, 'Cancelled by the client');
  }
}
