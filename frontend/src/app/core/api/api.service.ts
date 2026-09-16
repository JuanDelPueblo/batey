import { HttpClient, HttpErrorResponse } from '@angular/common/http';
import { inject, Service } from '@angular/core';
import { firstValueFrom } from 'rxjs';
import type {
  AgentAuthFlow,
  AgentAuthState,
  AgentManagementDetail,
  AgentSummary,
  Chat,
  CloneProjectInput,
  ConfigOption,
  CustomAgentInput,
  DirectoryListing,
  InstallRegistryAgentInput,
  Project,
  ChatWorkspaceSelection,
  RegistryCatalog,
  RemoveOutcome,
  UpdateOutcome,
  ValidationReport,
  WorkspaceOptions,
  ChatHistoryPage,
  TerminalTaskSummary,
  TerminalTaskDetails,
  RichContentBlock,
} from './types';

export interface AgentStatus {
  name: string;
  process_state: string;
  turn_state: string;
}

export interface StatusResponse {
  agents: AgentStatus[];
  project_root: string;
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
    readonly code?: string,
    readonly details?: Record<string, unknown>,
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

@Service()
export class ApiService {
  private readonly http = inject(HttpClient);

  private async request<T>(path: string, options: { method?: string; body?: unknown } = {}) {
    try {
      return await firstValueFrom(
        this.http.request<T>(options.method ?? 'GET', path, {
          body: options.body,
        }),
      );
    } catch (error) {
      if (error instanceof HttpErrorResponse) {
        let message = `Request failed: ${error.status} ${error.statusText}`;
        if (error.error && typeof error.error.error === 'string') {
          message = error.error.error;
        }
        const code = typeof error.error?.code === 'string' ? error.error.code : undefined;
        const details = error.error?.details && typeof error.error.details === 'object'
          ? error.error.details as Record<string, unknown>
          : undefined;
        throw new ApiError(error.status, message, code, details);
      }
      throw error;
    }
  }

  fetchProjects(): Promise<Project[]> {
    return this.request<Project[]>('/api/projects');
  }

  createProject(name: string, path: string): Promise<Project> {
    return this.request<Project>('/api/projects', {
      method: 'POST',
      body: { name, path },
    });
  }

  editProject(id: string, name: string, path: string): Promise<Project> {
    return this.request<Project>(`/api/projects/${encodeURIComponent(id)}`, {
      method: 'PATCH',
      body: { name, path },
    });
  }

  async deleteProject(id: string): Promise<void> {
    await this.request(`/api/projects/${encodeURIComponent(id)}`, { method: 'DELETE' });
  }

  cloneProject(input: CloneProjectInput): Promise<Project> {
    return this.request<Project>('/api/projects/clone', { method: 'POST', body: input });
  }

  fetchDirectories(path?: string): Promise<DirectoryListing> {
    const url = path
      ? `/api/filesystem/directories?path=${encodeURIComponent(path)}`
      : '/api/filesystem/directories';
    return this.request<DirectoryListing>(url);
  }

  fetchChats(projectId: string): Promise<Chat[]> {
    return this.request<Chat[]>(`/api/projects/${encodeURIComponent(projectId)}/chats`);
  }

  fetchWorkspaceOptions(projectId: string): Promise<WorkspaceOptions> {
    return this.request<WorkspaceOptions>(`/api/projects/${encodeURIComponent(projectId)}/workspace-options`);
  }

  createChat(projectId: string, agent: string, title?: string, workspace?: ChatWorkspaceSelection): Promise<Chat> {
    return this.request<Chat>(`/api/projects/${encodeURIComponent(projectId)}/chats`, {
      method: 'POST',
      body: { agent, title: title || undefined, ...(workspace ? { workspace } : {}) },
    });
  }

  fetchChat(chatId: string): Promise<Chat> {
    return this.request<Chat>(`/api/chats/${encodeURIComponent(chatId)}`);
  }

  fetchChatHistory(chatId: string, beforeSeq?: number, throughSeq?: number): Promise<ChatHistoryPage> {
    const params = new URLSearchParams();
    if (beforeSeq !== undefined) params.set('before_seq', String(beforeSeq));
    if (throughSeq !== undefined) params.set('through_seq', String(throughSeq));
    const query = params.toString() ? `?${params.toString()}` : '';
    return this.request<ChatHistoryPage>(
      `/api/chats/${encodeURIComponent(chatId)}/history${query}`,
    );
  }

  editChat(
    chatId: string,
    edit: { title?: string; archived?: boolean },
  ): Promise<Chat> {
    return this.request<Chat>(`/api/chats/${encodeURIComponent(chatId)}`, {
      method: 'PATCH',
      body: edit,
    });
  }

  async deleteChat(chatId: string): Promise<void> {
    await this.request(`/api/chats/${encodeURIComponent(chatId)}`, { method: 'DELETE' });
  }

  async promptChat(chatId: string, textOrContent: string | RichContentBlock[]): Promise<void> {
    await this.request(`/api/chats/${encodeURIComponent(chatId)}/prompt`, {
      method: 'POST',
      body: typeof textOrContent === 'string' ? { text: textOrContent } : { content: textOrContent },
    });
  }

  resumeChat(chatId: string): Promise<Chat> {
    return this.request<Chat>(`/api/chats/${encodeURIComponent(chatId)}/resume`, {
      method: 'POST',
    });
  }

  async authorizeChatEnvironment(chatId: string, remember = false): Promise<void> {
    await this.request(`/api/chats/${encodeURIComponent(chatId)}/environment/authorize`, {
      method: 'POST',
      body: { remember },
    });
  }

  async forgetProjectEnvrcGrant(projectId: string): Promise<void> {
    await this.request(`/api/projects/${encodeURIComponent(projectId)}/envrc-grant`, {
      method: 'DELETE',
    });
  }

  fetchChatTasks(chatId: string): Promise<TerminalTaskSummary[]> {
    return this.request<TerminalTaskSummary[]>(`/api/chats/${encodeURIComponent(chatId)}/tasks`);
  }

  fetchChatTask(chatId: string, taskId: string): Promise<TerminalTaskDetails> {
    return this.request<TerminalTaskDetails>(
      `/api/chats/${encodeURIComponent(chatId)}/tasks/${encodeURIComponent(taskId)}`,
    );
  }

  async stopChatTask(chatId: string, taskId: string): Promise<void> {
    await this.request(
      `/api/chats/${encodeURIComponent(chatId)}/tasks/${encodeURIComponent(taskId)}/stop`,
      { method: 'POST' },
    );
  }

  async stopChat(chatId: string): Promise<void> {
    await this.request(`/api/chats/${encodeURIComponent(chatId)}/stop`, { method: 'POST' });
  }

  async cancelChat(chatId: string): Promise<void> {
    await this.request(`/api/chats/${encodeURIComponent(chatId)}/cancel`, { method: 'POST' });
  }

  async respondPermission(chatId: string, id: string, optionId: string): Promise<void> {
    await this.request(`/api/chats/${encodeURIComponent(chatId)}/permission`, {
      method: 'POST',
      body: { id, option_id: optionId },
    });
  }

  fetchChatConfig(chatId: string): Promise<ConfigOption[]> {
    return this.request<ConfigOption[]>(`/api/chats/${encodeURIComponent(chatId)}/config`);
  }

  setChatConfig(chatId: string, optionId: string, value: unknown): Promise<ConfigOption[]> {
    return this.request<ConfigOption[]>(`/api/chats/${encodeURIComponent(chatId)}/config`, {
      method: 'PATCH',
      body: { id: optionId, value },
    });
  }

  async clearSavedConfig(chatId: string, optionId: string): Promise<void> {
    await this.request(`/api/chats/${encodeURIComponent(chatId)}/config/${encodeURIComponent(optionId)}`, {
      method: 'DELETE',
    });
  }

  fetchMcpServers(chatId: string): Promise<import('./types').McpServer[]> { return this.request(`/api/chats/${encodeURIComponent(chatId)}/mcp-servers`); }
  createMcpServer(chatId: string, input: import('./types').McpServerInput): Promise<import('./types').McpServer[]> { return this.request(`/api/chats/${encodeURIComponent(chatId)}/mcp-servers`, { method: 'POST', body: input }); }
  editMcpServer(chatId: string, serverId: string, input: import('./types').McpServerInput): Promise<import('./types').McpServer[]> { return this.request(`/api/chats/${encodeURIComponent(chatId)}/mcp-servers/${encodeURIComponent(serverId)}`, { method: 'PATCH', body: input }); }
  deleteMcpServer(chatId: string, serverId: string): Promise<void> { return this.request(`/api/chats/${encodeURIComponent(chatId)}/mcp-servers/${encodeURIComponent(serverId)}`, { method: 'DELETE' }); }
  orderMcpServers(chatId: string, ids: string[]): Promise<import('./types').McpServer[]> { return this.request(`/api/chats/${encodeURIComponent(chatId)}/mcp-servers/order`, { method: 'PUT', body: { ids } }); }
  fetchAdditionalRoots(chatId: string): Promise<import('./types').AdditionalRoot[]> { return this.request(`/api/chats/${encodeURIComponent(chatId)}/additional-roots`); }
  setAdditionalRoots(chatId: string, projectIds: string[]): Promise<import('./types').AdditionalRoot[]> { return this.request(`/api/chats/${encodeURIComponent(chatId)}/additional-roots`, { method: 'PUT', body: { project_ids: projectIds } }); }

  fetchChatCommands(chatId: string): Promise<import('./types').AvailableCommand[]> {
    return this.request<import('./types').AvailableCommand[]>(`/api/chats/${encodeURIComponent(chatId)}/commands`);
  }

  fetchChatModes(chatId: string): Promise<import('./types').SessionModes | null> {
    return this.request<import('./types').SessionModes | null>(`/api/chats/${encodeURIComponent(chatId)}/modes`);
  }

  setChatMode(chatId: string, modeId: string): Promise<import('./types').SessionModes> {
    return this.request<import('./types').SessionModes>(`/api/chats/${encodeURIComponent(chatId)}/modes`, {
      method: 'PATCH',
      body: { mode_id: modeId },
    });
  }

  fetchChatUsage(chatId: string): Promise<import('./types').UsageInfo | null> {
    return this.request<import('./types').UsageInfo | null>(`/api/chats/${encodeURIComponent(chatId)}/usage`);
  }

  fetchSessionInfo(chatId: string): Promise<unknown> {
    return this.request<unknown>(`/api/chats/${encodeURIComponent(chatId)}/session-info`);
  }

  fetchElicitations(chatId: string): Promise<import('./types').ElicitationInfo[]> {
    return this.request<import('./types').ElicitationInfo[]>(`/api/chats/${encodeURIComponent(chatId)}/elicitations`);
  }

  async respondElicitation(chatId: string, id: string, action: string, content?: unknown): Promise<void> {
    await this.request(`/api/chats/${encodeURIComponent(chatId)}/elicitations/${encodeURIComponent(id)}/respond`, {
      method: 'POST',
      body: content !== undefined ? { action, content } : { action },
    });
  }

  fetchRemoteSessions(chatId: string): Promise<{ sessions: Array<{ sessionId: string; title?: string }>; nextCursor?: string | null }> {
    return this.request<{ sessions: Array<{ sessionId: string; title?: string }>; nextCursor?: string | null }>(
      `/api/chats/${encodeURIComponent(chatId)}/remote-sessions`,
    );
  }

  async deleteRemoteSession(chatId: string, remoteId: string): Promise<void> {
    await this.request(`/api/chats/${encodeURIComponent(chatId)}/remote-sessions/${encodeURIComponent(remoteId)}`, {
      method: 'DELETE',
    });
  }

  fetchAgents(): Promise<AgentSummary[]> {
    return this.request<AgentSummary[]>('/api/agents');
  }

  fetchAgentDetail(id: string): Promise<AgentManagementDetail> {
    return this.request<AgentManagementDetail>(`/api/agents/${encodeURIComponent(id)}`);
  }

  validateCustomAgent(input: CustomAgentInput): Promise<ValidationReport> {
    return this.request<ValidationReport>('/api/agents/validate', {
      method: 'POST',
      body: input,
    });
  }

  createCustomAgent(input: CustomAgentInput): Promise<AgentSummary> {
    return this.request<AgentSummary>('/api/agents', { method: 'POST', body: input });
  }

  editCustomAgent(id: string, input: CustomAgentInput): Promise<AgentSummary> {
    return this.request<AgentSummary>(`/api/agents/${encodeURIComponent(id)}`, {
      method: 'PATCH',
      body: input,
    });
  }

  fetchRegistry(query?: string, refresh = false): Promise<RegistryCatalog> {
    const params = new URLSearchParams();
    if (query && query.trim()) params.set('q', query.trim());
    if (refresh) params.set('refresh', 'true');
    const suffix = params.toString() ? `?${params.toString()}` : '';
    return this.request<RegistryCatalog>(`/api/agents/registry${suffix}`);
  }

  refreshRegistry(): Promise<RegistryCatalog> {
    return this.request<RegistryCatalog>('/api/agents/registry/refresh', { method: 'POST' });
  }

  installRegistryAgent(input: InstallRegistryAgentInput): Promise<AgentSummary> {
    return this.request<AgentSummary>('/api/agents/registry/install', {
      method: 'POST',
      body: input,
    });
  }

  updateRegistryAgent(id: string): Promise<UpdateOutcome> {
    return this.request<UpdateOutcome>(`/api/agents/${encodeURIComponent(id)}/update`, {
      method: 'POST',
    });
  }

  async removeAgent(id: string): Promise<RemoveOutcome> {
    return this.request<RemoveOutcome>(`/api/agents/${encodeURIComponent(id)}`, {
      method: 'DELETE',
    });
  }

  fetchAgentEnv(id: string): Promise<import('./types').AgentEnvPresence[]> {
    return this.request<import('./types').AgentEnvPresence[]>(
      `/api/agents/${encodeURIComponent(id)}/environment`,
    );
  }

  updateAgentEnv(
    id: string,
    edits: import('./types').AgentEnvEdit[],
  ): Promise<import('./types').AgentEnvPresence[]> {
    return this.request<import('./types').AgentEnvPresence[]>(
      `/api/agents/${encodeURIComponent(id)}/environment`,
      { method: 'PATCH', body: edits },
    );
  }

  /** A plain cache-only read. Never starts an agent process. */
  fetchAgentAuth(id: string): Promise<AgentAuthState> {
    return this.request<AgentAuthState>(`/api/agents/${encodeURIComponent(id)}/auth`);
  }

  /**
   * Explicit refresh: the only read path, besides an actual authentication
   * or session lifecycle event, that may start this agent's ACP process.
   */
  refreshAgentAuth(id: string): Promise<import('./types').AgentAuthRefreshResult> {
    return this.request<import('./types').AgentAuthRefreshResult>(
      `/api/agents/${encodeURIComponent(id)}/auth/refresh`,
      { method: 'POST' },
    );
  }

  authenticateAgent(id: string, methodId: string): Promise<AgentAuthState> {
    return this.request<AgentAuthState>(
      `/api/agents/${encodeURIComponent(id)}/auth/${encodeURIComponent(methodId)}`,
      { method: 'POST' },
    );
  }

  logoutAgent(id: string): Promise<AgentAuthState> {
    return this.request<AgentAuthState>(`/api/agents/${encodeURIComponent(id)}/logout`, {
      method: 'POST',
    });
  }

  startTerminalAuth(id: string, methodId: string): Promise<AgentAuthFlow> {
    return this.request<AgentAuthFlow>(
      `/api/agents/${encodeURIComponent(id)}/auth/terminal/${encodeURIComponent(methodId)}`,
      { method: 'POST' },
    );
  }

  fetchAgentAuthFlow(flowId: string): Promise<AgentAuthFlow> {
    return this.request<AgentAuthFlow>(`/api/agent-auth/${encodeURIComponent(flowId)}`);
  }

  async cancelAgentAuthFlow(flowId: string): Promise<AgentAuthFlow> {
    return this.request<AgentAuthFlow>(`/api/agent-auth/${encodeURIComponent(flowId)}/cancel`, {
      method: 'POST',
    });
  }

  startProtocolAuth(id: string, methodId: string): Promise<import('./types').ProtocolAuthFlow> {
    return this.request<import('./types').ProtocolAuthFlow>(
      `/api/agents/${encodeURIComponent(id)}/auth/protocol/${encodeURIComponent(methodId)}`,
      { method: 'POST' },
    );
  }

  fetchProtocolAuthFlow(flowId: string): Promise<import('./types').ProtocolAuthFlow> {
    return this.request<import('./types').ProtocolAuthFlow>(
      `/api/protocol-auth/${encodeURIComponent(flowId)}`,
    );
  }

  cancelProtocolAuthFlow(flowId: string): Promise<import('./types').ProtocolAuthFlow> {
    return this.request<import('./types').ProtocolAuthFlow>(
      `/api/protocol-auth/${encodeURIComponent(flowId)}/cancel`,
      { method: 'POST' },
    );
  }

  fetchProtocolAuthElicitations(
    flowId: string,
  ): Promise<import('./types').ProtocolAuthElicitation[]> {
    return this.request<import('./types').ProtocolAuthElicitation[]>(
      `/api/protocol-auth/${encodeURIComponent(flowId)}/elicitations`,
    );
  }

  async respondProtocolAuthElicitation(
    flowId: string,
    id: string,
    action: string,
    content?: unknown,
  ): Promise<void> {
    await this.request(
      `/api/protocol-auth/${encodeURIComponent(flowId)}/elicitations/${encodeURIComponent(id)}/respond`,
      { method: 'POST', body: content !== undefined ? { action, content } : { action } },
    );
  }

  /** The opaque flow id is the only value the browser sends to open the PTY. */
  agentAuthSocketUrl(flowId: string): string {
    const protocol = typeof window !== 'undefined' && window.location.protocol === 'https:' ? 'wss' : 'ws';
    const host = typeof window !== 'undefined' ? window.location.host : 'localhost';
    return `${protocol}://${host}/api/agent-auth/${encodeURIComponent(flowId)}/ws`;
  }

  fetchStatus(): Promise<StatusResponse> {
    return this.request<StatusResponse>('/api/status');
  }
}
