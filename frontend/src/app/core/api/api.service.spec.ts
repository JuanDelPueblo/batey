import { TestBed } from '@angular/core/testing';
import { provideHttpClient } from '@angular/common/http';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { describe, expect, it, beforeEach, afterEach } from 'vitest';
import { ApiService } from './api.service';

describe('ApiService', () => {
  let api: ApiService;
  let http: HttpTestingController;

  beforeEach(() => {
    TestBed.configureTestingModule({ providers: [provideHttpClient(), provideHttpClientTesting()] });
    api = TestBed.inject(ApiService);
    http = TestBed.inject(HttpTestingController);
  });

  afterEach(() => http.verify());

  it('preserves the project REST contract', async () => {
    const promise = api.fetchProjects();
    const request = http.expectOne('/api/projects');
    expect(request.request.method).toBe('GET');
    request.flush([{ id: 'p1', name: 'Hub', path: '/work', created_at: 'a', updated_at: 'b' }]);
    await expect(promise).resolves.toHaveLength(1);
  });

  it('loads rich installed-agent summaries from the catalog endpoint', async () => {
    const promise = api.fetchAgents();
    const request = http.expectOne('/api/agents');
    expect(request.request.method).toBe('GET');
    request.flush([{
      id: 'custom', display_name: 'Custom ACP', source: 'file',
      availability: 'available', usage_provider: null, metadata: { package: 'custom' },
    }]);
    await expect(promise).resolves.toEqual([expect.objectContaining({ id: 'custom', source: 'file' })]);
  });

  it('serializes prompt payloads and URL-encodes chat ids', async () => {
    const promise = api.promptChat('chat/a', 'hello');
    const request = http.expectOne('/api/chats/chat%2Fa/prompt');
    expect(request.request.method).toBe('POST');
    expect(request.request.body).toEqual({ text: 'hello' });
    request.flush(null);
    await expect(promise).resolves.toBeUndefined();
  });

  it('requests chat-scoped history with a sequence cursor', async () => {
    const firstPromise = api.fetchChatHistory('chat/a', undefined, 7);
    const first = http.expectOne('/api/chats/chat%2Fa/history?through_seq=7');
    expect(first.request.method).toBe('GET');
    first.flush({ events: [], next_cursor: 42, has_older: true });
    await expect(firstPromise).resolves.toMatchObject({ next_cursor: 42, has_older: true });

    const olderPromise = api.fetchChatHistory('chat/a', 42);
    const older = http.expectOne('/api/chats/chat%2Fa/history?before_seq=42');
    older.flush({ events: [], next_cursor: null, has_older: false });
    await expect(olderPromise).resolves.toMatchObject({ has_older: false });
  });

  it('fetches workspace options and sends the Phase 2 workspace selection', async () => {
    const optionsPromise = api.fetchWorkspaceOptions('git/project');
    const optionsRequest = http.expectOne('/api/projects/git%2Fproject/workspace-options');
    expect(optionsRequest.request.method).toBe('GET');
    optionsRequest.flush({ is_git: true, current_branch: 'main', head_sha: 'a'.repeat(40), dirty: false, branches: [] });
    await expect(optionsPromise).resolves.toMatchObject({ current_branch: 'main' });

    const chatPromise = api.createChat('git/project', 'codex', undefined, {
      mode: 'project_checkout', branch: 'feature/ui',
    });
    const chatRequest = http.expectOne('/api/projects/git%2Fproject/chats');
    expect(chatRequest.request.body).toEqual({
      agent: 'codex', title: undefined, workspace: { mode: 'project_checkout', branch: 'feature/ui' },
    });
    chatRequest.flush({ id: 'chat-1' });
    await expect(chatPromise).resolves.toMatchObject({ id: 'chat-1' });
  });

  it('preserves the agent management contract', async () => {
    const detailPromise = api.fetchAgentDetail('my/custom');
    const detail = http.expectOne('/api/agents/my%2Fcustom');
    expect(detail.request.method).toBe('GET');
    detail.flush({ id: 'my/custom', command: 'agent', args: [], env: {} });
    await expect(detailPromise).resolves.toMatchObject({ command: 'agent' });

    const validatePromise = api.validateCustomAgent({ id: 'a', command: 'b', args: [], env: {} });
    const validate = http.expectOne('/api/agents/validate');
    expect(validate.request.method).toBe('POST');
    validate.flush({ valid: true, issues: [] });
    await expect(validatePromise).resolves.toMatchObject({ valid: true });

    const registryPromise = api.fetchRegistry('native', true);
    const registry = http.expectOne('/api/agents/registry?q=native&refresh=true');
    registry.flush({ status: 'fresh', source_url: 's', host: 'h', rejected: [], agents: [] });
    await expect(registryPromise).resolves.toMatchObject({ status: 'fresh' });

    const installPromise = api.installRegistryAgent({ registry_id: 'native-agent', distribution: 'binary' });
    const install = http.expectOne('/api/agents/registry/install');
    expect(install.request.body).toEqual({ registry_id: 'native-agent', distribution: 'binary' });
    install.flush({ id: 'native-agent' });
    await expect(installPromise).resolves.toMatchObject({ id: 'native-agent' });

    const updatePromise = api.updateRegistryAgent('native-agent');
    const update = http.expectOne('/api/agents/native-agent/update');
    expect(update.request.method).toBe('POST');
    update.flush({ updated: false, from_version: '1', to_version: '1', agent: { id: 'native-agent' } });
    await expect(updatePromise).resolves.toMatchObject({ updated: false });

    const removePromise = api.removeAgent('native-agent');
    const remove = http.expectOne('/api/agents/native-agent');
    expect(remove.request.method).toBe('DELETE');
    remove.flush({ id: 'native-agent', deleted: true, retained_chats: 0 });
    await expect(removePromise).resolves.toMatchObject({ deleted: true });

    const envPromise = api.fetchAgentEnv('codex');
    const env = http.expectOne('/api/agents/codex/environment');
    expect(env.request.method).toBe('GET');
    env.flush([{ name: 'CODEX_API_KEY', present: true }]);
    await expect(envPromise).resolves.toEqual([{ name: 'CODEX_API_KEY', present: true }]);

    const savePromise = api.updateAgentEnv('codex', [{ name: 'CODEX_API_KEY', action: 'replace', value: 'secret' }]);
    const save = http.expectOne('/api/agents/codex/environment');
    expect(save.request.method).toBe('PATCH');
    expect(save.request.body).toEqual([{ name: 'CODEX_API_KEY', action: 'replace', value: 'secret' }]);
    save.flush([{ name: 'CODEX_API_KEY', present: true }]);
    await expect(savePromise).resolves.toEqual([{ name: 'CODEX_API_KEY', present: true }]);
  });

  it('preserves the T111 authentication contract', async () => {
    const statePromise = api.fetchAgentAuth('codex');
    const stateRequest = http.expectOne('/api/agents/codex/auth');
    expect(stateRequest.request.method).toBe('GET');
    stateRequest.flush({
      agent_id: 'codex',
      methods: [{ id: 'api-key', name: 'API Key', type: 'terminal', supported: true }],
      logout_supported: true,
      terminal_supported: true,
    });
    await expect(statePromise).resolves.toMatchObject({ logout_supported: true, terminal_supported: true });

    const refreshPromise = api.refreshAgentAuth('codex');
    const refreshRequest = http.expectOne('/api/agents/codex/auth/refresh');
    expect(refreshRequest.request.method).toBe('POST');
    refreshRequest.flush({
      agent_id: 'codex',
      methods: [],
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'unknown',
      freshness: 'fresh',
    });
    await expect(refreshPromise).resolves.toMatchObject({ freshness: 'fresh' });

    const loginPromise = api.authenticateAgent('codex', 'openai');
    const login = http.expectOne('/api/agents/codex/auth/openai');
    expect(login.request.method).toBe('POST');
    login.flush({
      agent_id: 'codex',
      methods: [],
      logout_supported: true,
      terminal_supported: true,
    });
    await expect(loginPromise).resolves.toMatchObject({ logout_supported: true });

    const logoutPromise = api.logoutAgent('codex');
    const logout = http.expectOne('/api/agents/codex/logout');
    expect(logout.request.method).toBe('POST');
    logout.flush({
      agent_id: 'codex',
      methods: [],
      logout_supported: true,
      terminal_supported: true,
    });
    await expect(logoutPromise).resolves.toMatchObject({ logout_supported: true });

    const flowPromise = api.startTerminalAuth('codex', 'api-key');
    const flow = http.expectOne('/api/agents/codex/auth/terminal/api-key');
    expect(flow.request.method).toBe('POST');
    flow.flush({ flow_id: 'flow-1', agent_id: 'codex', method_id: 'api-key', state: 'running' });
    await expect(flowPromise).resolves.toMatchObject({ flow_id: 'flow-1' });

    const getFlowPromise = api.fetchAgentAuthFlow('flow-1');
    const getFlow = http.expectOne('/api/agent-auth/flow-1');
    expect(getFlow.request.method).toBe('GET');
    getFlow.flush({ flow_id: 'flow-1', agent_id: 'codex', method_id: 'api-key', state: 'succeeded' });
    await expect(getFlowPromise).resolves.toMatchObject({ state: 'succeeded' });

    const cancelPromise = api.cancelAgentAuthFlow('flow-1');
    const cancel = http.expectOne('/api/agent-auth/flow-1/cancel');
    expect(cancel.request.method).toBe('POST');
    cancel.flush({ flow_id: 'flow-1', agent_id: 'codex', method_id: 'api-key', state: 'cancelled' });
    await expect(cancelPromise).resolves.toMatchObject({ state: 'cancelled' });

    expect(api.agentAuthSocketUrl('flow-1')).toContain('/api/agent-auth/flow-1/ws');
  });
});
