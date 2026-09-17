import { computed, inject, Service } from '@angular/core';
import { NavigationEnd, Router } from '@angular/router';
import { filter } from 'rxjs';
import { EventSocketService } from '../core/event-socket.service';
import type { ChatWorkspaceSelection, CloneProjectInput, RichContentBlock, SessionEvent } from '../core/api/types';
import { EventReducer } from './event-reducer';
import type { ChatActivity } from './chat-activity';
import { AgentStore } from './agent.store';
import { ChatSessionStore } from './chat-session.store';
import { ProjectStore } from './project.store';
import { UiStateStore } from './ui-state.store';

/**
 * Coordinates startup, routing, and cross-feature operations. State ownership
 * lives in the project, chat/session, and UI stores; aliases preserve the
 * established view-facing API while components migrate independently.
 */
@Service()
export class AppStateService {
  readonly projectStore = inject(ProjectStore);
  readonly chatStore = inject(ChatSessionStore);
  readonly agentStore = inject(AgentStore);
  readonly uiStore = inject(UiStateStore);

  readonly projects = this.projectStore.projects;
  readonly agents = this.agentStore.installed;
  readonly agentError = this.agentStore.error;
  readonly agentsLoading = this.agentStore.loading;
  readonly registry = this.agentStore.registry;
  readonly registryLoading = this.agentStore.registryLoading;
  readonly registryError = this.agentStore.registryError;
  readonly authByAgent = this.agentStore.authByAgent;
  readonly authLoading = this.agentStore.authLoading;
  readonly authErrors = this.agentStore.authErrors;
  readonly protocolFlowsByAgent = this.agentStore.protocolFlowsByAgent;
  readonly protocolElicitationsByFlow = this.agentStore.protocolElicitationsByFlow;
  readonly protocolInteractionsByFlow = this.agentStore.protocolInteractionsByFlow;
  readonly protocolLoading = this.agentStore.protocolLoading;
  readonly terminalFlowsByAgent = this.agentStore.terminalFlowsByAgent;
  readonly operationsByAgent = this.agentStore.operationsByAgent;
  readonly loadingProjects = this.projectStore.loading;
  readonly projectsError = this.projectStore.error;
  readonly chatsByProject = this.chatStore.chatsByProject;
  readonly configOptionsByChat = this.chatStore.configOptionsByChat;
  readonly configLoadedByChat = this.chatStore.configLoadedByChat;
  readonly commandsByChat = this.chatStore.commandsByChat;
  readonly modesByChat = this.chatStore.modesByChat;
  readonly usageByChat = this.chatStore.usageByChat;
  readonly elicitationsByChat = this.chatStore.elicitationsByChat;
  readonly reducersByChat = this.chatStore.reducersByChat;
  readonly loadingChats = this.chatStore.loadingChats;
  readonly connectingChats = this.chatStore.connectingChats;
  readonly connectErrors = this.chatStore.connectErrors;
  readonly rejectedConfigByChat = this.chatStore.rejectedConfigByChat;
  readonly blockedEnvrcByChat = this.chatStore.blockedEnvrcByChat;
  readonly authRequiredByChat = this.chatStore.authRequiredByChat;
  readonly historyLoadingByChat = this.chatStore.historyLoadingByChat;
  readonly historyHasOlderByChat = this.chatStore.historyHasOlderByChat;
  readonly historyErrors = this.chatStore.historyErrors;
  readonly activeProjectId = this.uiStore.activeProjectId;
  readonly activeChatId = this.uiStore.activeChatId;
  readonly isMobileDrawerOpen = this.uiStore.isMobileDrawerOpen;
  readonly showArchived = this.uiStore.showArchived;

  private readonly socket = inject(EventSocketService);
  private readonly router = inject(Router);
  private readonly emptyReducer = new EventReducer();

  readonly wsStatus = this.socket.status;
  readonly wsError = this.socket.errorMessage;
  readonly activeProject = computed(() => {
    const id = this.activeProjectId();
    return id ? this.projects().find((project) => project.id === id) ?? null : null;
  });
  readonly activeChat = computed(() => {
    const projectId = this.activeProjectId();
    const chatId = this.activeChatId();
    if (!projectId || !chatId) return null;
    return this.chatsByProject()[projectId]?.find((chat) => chat.id === chatId) ?? null;
  });
  readonly activeReducer = computed(() => {
    const chatId = this.activeChatId();
    return chatId ? this.reducersByChat()[chatId] ?? this.emptyReducer : this.emptyReducer;
  });

  constructor() {
    this.socket.events.subscribe((event) => this.handleIncomingEvent(event));
    this.socket.replayGaps.subscribe(() => this.chatStore.resetEventHistory());
    this.router.events
      .pipe(filter((event): event is NavigationEnd => event instanceof NavigationEnd))
      .subscribe((event) => this.syncRoute(event.urlAfterRedirects));
    this.syncRoute(this.router.url || (typeof window !== 'undefined' ? window.location.pathname : '/'));
    this.socket.connect();
    void this.initialize();
  }

  setMobileDrawerOpen(open: boolean): void { this.uiStore.setMobileDrawerOpen(open); }
  setShowArchived(show: boolean): void { this.uiStore.setShowArchived(show); }
  loadProjects(): Promise<void> { return this.projectStore.loadProjects(); }
  loadAgents(): Promise<void> { return this.agentStore.loadInstalled(); }
  loadRegistry(): Promise<void> { return this.agentStore.loadRegistry(); }
  refreshRegistry(): Promise<void> { return this.agentStore.refreshRegistry(); }
  operationForAgent(key: string) { return this.agentStore.operationForAgent(key); }
  isAgentBusy(key: string) { return this.agentStore.isAgentBusy(key); }
  installRegistryAgent(input: import('../core/api/types').InstallRegistryAgentInput) { return this.agentStore.installRegistryAgent(input); }
  updateAgent(id: string) { return this.agentStore.updateAgent(id); }
  removeAgent(id: string) { return this.agentStore.removeAgent(id); }
  fetchAgentDetail(id: string) { return this.agentStore.fetchDetail(id); }
  loadAgentEnv(id: string) { return this.agentStore.loadAgentEnv(id); }
  updateAgentEnv(id: string, edits: import('../core/api/types').AgentEnvEdit[]) {
    return this.agentStore.updateAgentEnv(id, edits);
  }
  validateCustomAgent(input: import('../core/api/types').CustomAgentInput) { return this.agentStore.validateCustomAgent(input); }
  createCustomAgent(input: import('../core/api/types').CustomAgentInput) { return this.agentStore.createCustomAgent(input); }
  editCustomAgent(id: string, input: import('../core/api/types').CustomAgentInput) { return this.agentStore.editCustomAgent(id, input); }
  loadAgentAuth(id: string) { return this.agentStore.loadAuth(id); }
  refreshAgentAuth(id: string) { return this.agentStore.refreshAuth(id); }
  authenticateAgent(id: string, methodId: string) { return this.agentStore.authenticate(id, methodId); }
  logoutAgent(id: string) { return this.agentStore.logout(id); }
  startTerminalAgentAuth(id: string, methodId: string) { return this.agentStore.startTerminalAuth(id, methodId); }
  fetchTerminalAgentFlow(flowId: string) { return this.agentStore.fetchTerminalFlow(flowId); }
  setTerminalAgentFlow(id: string, flow: import('../core/api/types').AgentAuthFlow | null) { return this.agentStore.setTerminalFlow(id, flow); }
  cancelTerminalAgentAuth(id: string, flowId: string) { return this.agentStore.cancelTerminalFlow(id, flowId); }
  startProtocolAgentAuth(id: string, methodId: string) { return this.agentStore.startProtocolAuth(id, methodId); }
  setProtocolFlowFromActive(id: string, active: import('../core/api/types').ActiveAuthFlow) { return this.agentStore.setProtocolFlowFromActive(id, active); }
  refreshProtocolAgentAuth(agentId: string, flowId: string) { return this.agentStore.refreshProtocolFlow(agentId, flowId); }
  cancelProtocolAgentAuth(agentId: string, flowId: string) { return this.agentStore.cancelProtocolAuth(agentId, flowId); }
  relayProtocolAuthCallback(flowId: string, callbackUrl: string) { return this.agentStore.relayProtocolCallback(flowId, callbackUrl); }
  clearProtocolAgentAuth(agentId: string) { return this.agentStore.clearProtocolFlow(agentId); }
  respondProtocolElicitation(flowId: string, id: string, action: string, content?: unknown) { return this.agentStore.respondProtocolElicitation(flowId, id, action, content); }
  clearAuthRequired(chatId: string): void { this.chatStore.clearAuthRequired(chatId); }
  loadChats(projectId: string): Promise<void> { return this.chatStore.loadChats(projectId); }
  findChat(chatId: string) { return this.chatStore.findChat(chatId); }
  chatActivity(chatId: string): ChatActivity { return this.chatStore.chatActivity(chatId); }
  chatTurnStartedAt(chatId: string): string | null { return this.chatStore.chatTurnStartedAt(chatId); }
  autoConnectChat(chatId: string): Promise<void> { return this.chatStore.autoConnectChat(chatId); }
  loadChatHistory(chatId: string): Promise<void> { return this.chatStore.loadChatHistory(chatId); }
  loadOlderHistory(chatId: string): Promise<void> { return this.chatStore.loadOlderHistory(chatId); }
  retryHistory(chatId: string): Promise<void> { return this.chatStore.retryHistory(chatId); }
  loadChatConfig(chatId: string) { return this.chatStore.loadChatConfig(chatId); }
  retryConnection(chatId: string): Promise<void> { return this.chatStore.retryConnection(chatId); }
  async authorizeChatEnvironment(chatId: string, remember = false): Promise<void> {
    const { remembered, projectId, relativePath } = await this.chatStore.authorizeChatEnvironment(
      chatId,
      remember,
    );
    if (remembered && projectId) {
      this.projectStore.patchEnvrcState(projectId, true, relativePath);
    }
  }
  forgetProjectEnvrcGrant(projectId: string): Promise<void> {
    return this.projectStore.forgetProjectEnvrcGrant(projectId);
  }
  resetRejectedConfig(chatId: string): Promise<void> { return this.chatStore.resetRejectedConfig(chatId); }
  connectChat(chatId: string) { return this.chatStore.connectChat(chatId); }
  fetchConfig(chatId: string) { return this.chatStore.fetchConfig(chatId); }
  sendPrompt(chatId: string, text: string | RichContentBlock[]): Promise<void> { return this.chatStore.sendPrompt(chatId, text); }
  cancelActiveTurn(chatId: string): Promise<void> { return this.chatStore.cancelActiveTurn(chatId); }
  stopChatProcess(chatId: string): Promise<void> { return this.chatStore.stopChatProcess(chatId); }
  renameChat(chatId: string, title: string): Promise<void> { return this.chatStore.renameChat(chatId, title); }
  archiveChat(chatId: string, archived: boolean): Promise<void> { return this.chatStore.archiveChat(chatId, archived); }

  async deleteChat(chatId: string): Promise<void> {
    const chat = await this.chatStore.deleteChat(chatId);
    if (this.activeChatId() === chatId) {
      void this.router.navigate(chat?.project_id ? ['/projects', chat.project_id] : ['/']);
    }
  }

  setChatConfig(chatId: string, optionId: string, value: unknown): Promise<void> {
    return this.chatStore.setChatConfig(chatId, optionId, value);
  }

  createProject(name: string, path: string) { return this.projectStore.createProject(name, path); }
  cloneProject(input: CloneProjectInput) { return this.projectStore.cloneProject(input); }

  async createChat(projectId: string, agent: string, title?: string, workspace?: ChatWorkspaceSelection) {
    const created = await this.chatStore.createChat(projectId, agent, title, workspace);
    this.projectStore.incrementChatCount(projectId);
    return created;
  }

  editProject(id: string, name: string, path: string) {
    return this.projectStore.editProject(id, name, path);
  }

  async deleteProject(id: string): Promise<void> {
    await this.projectStore.deleteProject(id);
    this.chatStore.removeProject(id);
    if (this.activeProjectId() === id) {
      this.uiStore.setRoute(null, null);
      void this.router.navigate(['/']);
    }
  }

  respondPermission(chatId: string, requestId: string, optionId: string): Promise<void> {
    return this.chatStore.respondPermission(chatId, requestId, optionId);
  }

  loadChatCommands(chatId: string): Promise<void> { return this.chatStore.loadChatCommands(chatId); }
  loadChatModes(chatId: string): Promise<void> { return this.chatStore.loadChatModes(chatId); }
  setChatMode(chatId: string, modeId: string): Promise<void> { return this.chatStore.setChatMode(chatId, modeId); }
  loadChatUsage(chatId: string): Promise<void> { return this.chatStore.loadChatUsage(chatId); }
  respondElicitation(chatId: string, id: string, action: string, content?: unknown): Promise<void> {
    return this.chatStore.respondElicitation(chatId, id, action, content);
  }
  deleteRemoteSession(chatId: string, remoteId: string): Promise<void> {
    return this.chatStore.deleteRemoteSession(chatId, remoteId);
  }

  private async initialize(): Promise<void> {
    if (typeof window === 'undefined') return;
    await Promise.all([this.loadProjects(), this.loadAgents()]);
  }

  private syncRoute(url: string): void {
    const parts = url.split('?')[0].replace(/\/+$/, '').split('/').filter(Boolean);
    const projectId = parts[0] === 'projects' ? parts[1] ?? null : null;
    const chatId = projectId && parts[2] === 'chats' ? parts[3] ?? null : null;
    this.uiStore.setRoute(projectId, chatId);
    if (!projectId) {
      this.setMobileDrawerOpen(false);
      return;
    }
    void this.loadChats(projectId).then(() => {
      if (chatId) void this.autoConnectChat(chatId);
    });
  }

  private handleIncomingEvent(event: SessionEvent): void {
    if (event.payload.type === 'metadata_changed') {
      void this.loadProjects();
      for (const projectId of Object.keys(this.chatsByProject())) void this.loadChats(projectId);
      return;
    }
    this.chatStore.handleIncomingEvent(event);
  }
}
