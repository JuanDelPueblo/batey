import { TestBed } from '@angular/core/testing';
import { signal } from '@angular/core';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ApiService } from '../core/api/api.service';
import type { AgentAuthState, AgentOperation, AgentSummary, ProtocolAuthElicitation, ProtocolAuthFlow } from '../core/api/types';
import { AgentStore } from './agent.store';
import { ProjectStore } from './project.store';

function makeApi() {
  return {
    fetchAgents: vi.fn(async () => [] as AgentSummary[]),
    fetchRegistry: vi.fn(async () => ({ status: 'cached', source_url: 's', host: 'h', rejected: [], agents: [] })),
    refreshRegistry: vi.fn(async () => ({ status: 'fresh', source_url: 's', host: 'h', rejected: [], agents: [] })),
    installRegistryAgent: vi.fn(async () => ({
      id: 'op-install-1',
      agent_id: 'native-agent',
      registry_id: 'native-agent',
      kind: 'install',
      state: 'succeeded',
      stage: 'completed',
      downloaded_bytes: 100,
      total_bytes: 100,
      error: null,
      started_at: '2026-01-01T00:00:00Z',
      completed_at: '2026-01-01T00:00:01Z',
    } as AgentOperation)),
    updateRegistryAgent: vi.fn(async () => ({
      id: 'op-update-1',
      agent_id: 'native-agent',
      registry_id: 'native-agent',
      kind: 'update',
      state: 'succeeded',
      stage: 'completed',
      downloaded_bytes: 100,
      total_bytes: 100,
      error: null,
      started_at: '2026-01-01T00:00:00Z',
      completed_at: '2026-01-01T00:00:01Z',
      to_version: '2',
    } as AgentOperation)),
    getAgentOperation: vi.fn(async (id: string) => ({
      id,
      agent_id: 'native-agent',
      registry_id: 'native-agent',
      kind: 'install',
      state: 'succeeded',
      stage: 'completed',
      downloaded_bytes: 100,
      total_bytes: 100,
      error: null,
      started_at: '2026-01-01T00:00:00Z',
      completed_at: '2026-01-01T00:00:01Z',
    } as AgentOperation)),
    listAgentOperations: vi.fn(async () => [] as AgentOperation[]),
    removeAgent: vi.fn(async () => ({ id: 'a', deleted: true, retained_chats: 0 })),
    fetchAgentDetail: vi.fn(async () => ({ id: 'a', command: 'c', args: [], env: {} })),
    validateCustomAgent: vi.fn(async () => ({ valid: true, issues: [] })),
    createCustomAgent: vi.fn(async () => ({ id: 'a' } as AgentSummary)),
    editCustomAgent: vi.fn(async () => ({ id: 'a' } as AgentSummary)),
    fetchAgentAuth: vi.fn(async (id: string): Promise<AgentAuthState> => ({
      agent_id: id,
      methods: [],
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'unknown',
      freshness: 'cached',
      observed_freshness: 'cached',
    })),
    refreshAgentAuth: vi.fn(async (id: string): Promise<import('../core/api/types').AgentAuthRefreshResult> => ({
      agent_id: id,
      methods: [],
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'unknown',
      freshness: 'fresh',
      observed_freshness: 'fresh',
    })),
    authenticateAgent: vi.fn(async (id: string): Promise<AgentAuthState> => ({
      agent_id: id,
      methods: [],
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'authenticated',
      freshness: 'fresh',
      observed_freshness: 'fresh',
    })),
    logoutAgent: vi.fn(async (id: string): Promise<AgentAuthState> => ({
      agent_id: id,
      methods: [],
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'authentication_required',
      freshness: 'fresh',
      observed_freshness: 'fresh',
    })),
    startTerminalAuth: vi.fn(async () => ({
      flow_id: 'f',
      agent_id: 'a',
      method_id: 'm',
      state: 'running' as const,
    })),
    fetchAgentEnv: vi.fn(async () => [{ name: 'CODEX_API_KEY', present: true }]),
    updateAgentEnv: vi.fn(async () => [{ name: 'CODEX_API_KEY', present: true }]),
    startProtocolAuth: vi.fn(async () => ({
      flow_id: 'p',
      agent_id: 'a',
      method_id: 'm',
      state: 'running' as const,
    })),
    fetchProtocolAuthFlow: vi.fn(async (flowId: string): Promise<ProtocolAuthFlow> => ({
      flow_id: flowId,
      agent_id: 'a',
      method_id: 'm',
      state: 'succeeded',
      reason: null,
      started_at: new Date().toISOString(),
      completed_at: null,
    })),
    fetchProtocolAuthElicitations: vi.fn(async (): Promise<ProtocolAuthElicitation[]> => []),
    fetchProtocolAuthInteraction: vi.fn(async () => null),
    cancelProtocolAuthFlow: vi.fn(async (flowId: string) => ({
      flow_id: flowId,
      agent_id: 'a',
      method_id: 'm',
      state: 'cancelled' as const,
    })),
    respondProtocolAuthElicitation: vi.fn(async () => undefined),
  };
}

describe('AgentStore', () => {
  let store: AgentStore;
  let api: ReturnType<typeof makeApi>;
  let agents: ReturnType<typeof signal<AgentSummary[]>>;

  beforeEach(() => {
    api = makeApi();
    agents = signal<AgentSummary[]>([]);
    TestBed.configureTestingModule({
      providers: [
        { provide: ApiService, useValue: api },
        { provide: ProjectStore, useValue: { agents } },
      ],
    });
    store = TestBed.inject(AgentStore);
  });

  it('owns the installed catalog through the shared agent signal', async () => {
    const summary = { id: 'codex', display_name: 'Codex', source: 'builtin', availability: 'available', metadata: {} } as AgentSummary;
    api.fetchAgents.mockResolvedValueOnce([summary]);
    await store.loadInstalled();
    expect(store.installed()).toEqual([summary]);
    expect(agents()).toEqual([summary]);
    expect(store.error()).toBeNull();
  });

  it('records a catalog load failure without inventing entries', async () => {
    api.fetchAgents.mockRejectedValueOnce(new Error('catalog down'));
    await store.loadInstalled();
    expect(store.installed()).toEqual([]);
    expect(store.error()).toBe('catalog down');
  });

  it('hydrates active operations and resumes polling after initialization', async () => {
    const active: AgentOperation = {
      id: 'op-reload-1',
      agent_id: 'native-agent',
      registry_id: 'native-agent',
      kind: 'install',
      state: 'running',
      stage: 'downloading',
      downloaded_bytes: 50,
      total_bytes: 100,
      error: null,
      started_at: '2026-01-01T00:00:00Z',
      completed_at: null,
    };
    api.listAgentOperations.mockResolvedValueOnce([active]);
    api.getAgentOperation.mockResolvedValueOnce({
      ...active,
      state: 'succeeded',
      stage: 'completed',
      downloaded_bytes: 100,
      completed_at: '2026-01-01T00:00:01Z',
    });

    await store.loadInstalled();
    expect(api.listAgentOperations).toHaveBeenCalled();
    expect(api.getAgentOperation).toHaveBeenCalledWith('op-reload-1');
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(store.isAgentBusy('native-agent')).toBe(false);
  });

  it('loads and refreshes the registry catalog', async () => {
    await store.loadRegistry();
    expect(api.fetchRegistry).toHaveBeenCalledWith();
    await store.refreshRegistry();
    expect(api.refreshRegistry).toHaveBeenCalled();
    expect(store.registry()?.status).toBe('fresh');
  });

  it('keeps the fresh catalog from the first registry load', async () => {
    api.fetchRegistry.mockResolvedValueOnce({
      status: 'fresh',
      source_url: 's',
      host: 'h',
      rejected: [],
      agents: [],
    });
    await store.loadRegistry();
    expect(store.registry()?.status).toBe('fresh');
    expect(store.registryError()).toBeNull();
  });

  it('installs, updates, and removes through the API and reloads the catalog', async () => {
    await store.installRegistryAgent({ registry_id: 'native-agent' });
    expect(api.installRegistryAgent).toHaveBeenCalledWith({ registry_id: 'native-agent' });
    expect(api.fetchAgents).toHaveBeenCalled();

    await store.updateAgent('native-agent');
    expect(api.updateRegistryAgent).toHaveBeenCalledWith('native-agent');

    await store.removeAgent('native-agent');
    expect(api.removeAgent).toHaveBeenCalledWith('native-agent');
  });

  it('tracks in-flight operation progress and clears on success', async () => {
    let callCount = 0;
    api.installRegistryAgent.mockResolvedValueOnce({
      id: 'op-poll-1',
      agent_id: 'native-agent',
      registry_id: 'native-agent',
      kind: 'install',
      state: 'running',
      stage: 'downloading',
      downloaded_bytes: 50,
      total_bytes: 100,
      error: null,
      started_at: '2026-01-01T00:00:00Z',
      completed_at: null,
    });
    api.getAgentOperation.mockImplementation(async () => {
      callCount++;
      if (callCount === 1) {
        return {
          id: 'op-poll-1',
          agent_id: 'native-agent',
          registry_id: 'native-agent',
          kind: 'install',
          state: 'running',
          stage: 'verifying',
          downloaded_bytes: 100,
          total_bytes: 100,
          error: null,
          started_at: '2026-01-01T00:00:00Z',
          completed_at: null,
        };
      }
      return {
        id: 'op-poll-1',
        agent_id: 'native-agent',
        registry_id: 'native-agent',
        kind: 'install',
        state: 'succeeded',
        stage: 'completed',
        downloaded_bytes: 100,
        total_bytes: 100,
        error: null,
        started_at: '2026-01-01T00:00:00Z',
        completed_at: '2026-01-01T00:00:01Z',
      };
    });

    const installPromise = store.installRegistryAgent({ registry_id: 'native-agent' });
    await Promise.resolve();
    expect(store.isAgentBusy('native-agent')).toBe(true);

    const outcome = await installPromise;
    expect(outcome.state).toBe('succeeded');
    expect(store.isAgentBusy('native-agent')).toBe(false);
  });

  it('clears operation and throws on operation failure', async () => {
    api.installRegistryAgent.mockResolvedValueOnce({
      id: 'op-fail',
      agent_id: 'fail-agent',
      registry_id: 'fail-agent',
      kind: 'install',
      state: 'running',
      stage: 'downloading',
      downloaded_bytes: 10,
      total_bytes: 100,
      error: null,
      started_at: '2026-01-01T00:00:00Z',
      completed_at: null,
    });
    api.getAgentOperation.mockResolvedValueOnce({
      id: 'op-fail',
      agent_id: 'fail-agent',
      registry_id: 'fail-agent',
      kind: 'install',
      state: 'failed',
      stage: 'failed',
      downloaded_bytes: 10,
      total_bytes: 100,
      error: 'SHA-256 digest mismatch',
      started_at: '2026-01-01T00:00:00Z',
      completed_at: '2026-01-01T00:00:01Z',
    });

    await expect(store.installRegistryAgent({ registry_id: 'fail-agent' })).rejects.toThrow('SHA-256 digest mismatch');
    expect(store.isAgentBusy('fail-agent')).toBe(false);
  });

  it('validates and persists custom definitions', async () => {
    const input = { id: 'a', command: 'c', args: [], env: {} };
    await store.validateCustomAgent(input);
    expect(api.validateCustomAgent).toHaveBeenCalledWith(input);

    await store.createCustomAgent(input);
    expect(api.createCustomAgent).toHaveBeenCalledWith(input);

    await store.editCustomAgent('a', input);
    expect(api.editCustomAgent).toHaveBeenCalledWith('a', input);
  });

  it('tracks provider-neutral authentication state', async () => {
    await store.loadAuth('codex');
    expect(store.authByAgent()['codex'].logout_supported).toBe(true);

    api.authenticateAgent.mockResolvedValueOnce({
      agent_id: 'codex',
      methods: [{ id: 'm1', name: 'M1', type: 'agent', supported: true }],
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'authenticated',
      freshness: 'fresh',
      observed_freshness: 'fresh',
    });
    await store.authenticate('codex', 'openai');
    expect(api.authenticateAgent).toHaveBeenCalledWith('codex', 'openai');
    expect(store.authByAgent()['codex'].methods.length).toBe(1);
    expect(store.authByAgent()['codex'].observed_state).toBe('authenticated');

    await store.logout('codex');
    expect(api.logoutAgent).toHaveBeenCalledWith('codex');
    expect(store.authByAgent()['codex'].logout_supported).toBe(true);
  });

  it('refreshes authentication state explicitly and surfaces a probe failure without losing the cache', async () => {
    await store.refreshAuth('codex');
    expect(api.refreshAgentAuth).toHaveBeenCalledWith('codex');
    expect(store.authByAgent()['codex'].freshness).toBe('fresh');
    expect(store.authErrors()['codex']).toBeUndefined();

    api.refreshAgentAuth.mockResolvedValueOnce({
      agent_id: 'codex',
      methods: [{ id: 'm1', name: 'M1', type: 'agent', supported: true }],
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'unknown',
      freshness: 'stale',
      observed_freshness: 'stale',
      refresh_error: "Agent 'codex' did not start in time",
    });
    const state = await store.refreshAuth('codex');
    // The last known methods stay in the store; the failure never erases them.
    expect(state.methods.length).toBe(1);
    expect(store.authByAgent()['codex'].methods.length).toBe(1);
    expect(store.authErrors()['codex']).toBe("Agent 'codex' did not start in time");

    // Retrying refresh immediately clears the prior authError while in flight
    let inFlightError: string | undefined = "initial";
    api.refreshAgentAuth.mockImplementationOnce(async () => {
      inFlightError = store.authErrors()['codex'];
      return {
        agent_id: 'codex',
        methods: [{ id: 'm1', name: 'M1', type: 'agent', supported: true }],
        logout_supported: true,
        terminal_supported: true,
        observed_state: 'authenticated',
        freshness: 'fresh',
        observed_freshness: 'fresh',
      };
    });
    await store.refreshAuth('codex');
    expect(inFlightError).toBeUndefined();
    expect(store.authErrors()['codex']).toBeUndefined();
  });

  it('clears elicitations when cancelling a protocol flow', async () => {
    api.fetchProtocolAuthElicitations.mockResolvedValueOnce([
      { id: 'el-1', mode: 'url', message: 'visit', schema: null, url: 'https://example.com' },
    ]);
    api.fetchProtocolAuthFlow.mockResolvedValueOnce({
      flow_id: 'flow-1',
      agent_id: 'a',
      method_id: 'm',
      state: 'waiting_for_user',
      reason: null,
      started_at: new Date().toISOString(),
      completed_at: null,
    });
    await store.refreshProtocolFlow('a', 'flow-1');
    expect(store.protocolElicitationsByFlow()['flow-1']).toHaveLength(1);

    await store.cancelProtocolAuth('a', 'flow-1');
    expect(store.protocolElicitationsByFlow()['flow-1']).toEqual([]);
  });

  it('starts a terminal flow and refreshes state after it', async () => {
    const flow = await store.startTerminalAuth('codex', 'api-key');
    expect(flow.flow_id).toBe('f');
    expect(api.startTerminalAuth).toHaveBeenCalledWith('codex', 'api-key');
    expect(api.fetchAgentAuth).toHaveBeenCalledWith('codex');
  });

  it('loads and saves private environment as presence only', async () => {
    const presence = await store.loadAgentEnv('codex');
    expect(api.fetchAgentEnv).toHaveBeenCalledWith('codex');
    expect(presence).toEqual([{ name: 'CODEX_API_KEY', present: true }]);
    expect(store.envByAgent()['codex']).toEqual([{ name: 'CODEX_API_KEY', present: true }]);
    expect(JSON.stringify(store.envByAgent())).not.toContain('secret');

    const edits = [{ name: 'CODEX_API_KEY', action: 'replace' as const, value: 'secret' }];
    await store.updateAgentEnv('codex', edits);
    expect(api.updateAgentEnv).toHaveBeenCalledWith('codex', edits);
    // The stored response is presence only; values never enter frontend state.
    expect(JSON.stringify(store.envByAgent()['codex'])).not.toContain('secret');
  });
});
