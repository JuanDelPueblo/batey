import { inject, Service, signal, WritableSignal } from '@angular/core';
import { ApiError, ApiService } from '../core/api/api.service';
import { EventSocketService } from '../core/event-socket.service';
import type {
  AuthRequiredInfo,
  Chat,
  ConfigOption,
  ProcessState,
  SessionEvent,
  TurnState,
  ChatWorkspaceSelection,
} from '../core/api/types';
import { deriveChatActivity, type ChatActivity } from './chat-activity';
import { EventReducer } from './event-reducer';

type ChatMap = Record<string, Chat[]>;
type ConfigMap = Record<string, ConfigOption[]>;
type BooleanMap = Record<string, boolean>;
type ErrorMap = Record<string, string>;
type CursorMap = Record<string, number | null>;

export function compareChatsByRecency(left: Chat, right: Chat): number {
  const leftTime = Date.parse(left.updated_at);
  const rightTime = Date.parse(right.updated_at);
  const validLeft = Number.isFinite(leftTime) ? leftTime : Number.NEGATIVE_INFINITY;
  const validRight = Number.isFinite(rightTime) ? rightTime : Number.NEGATIVE_INFINITY;
  return validRight - validLeft || right.id.localeCompare(left.id);
}

/** Owns chat collections, ACP session state, configuration, and event reduction. */
@Service()
export class ChatSessionStore {
  private readonly api = inject(ApiService);
  private readonly socket = inject(EventSocketService);

  readonly chatsByProject = signal<ChatMap>({});
  readonly configOptionsByChat = signal<ConfigMap>({});
  readonly configLoadedByChat = signal<BooleanMap>({});
  readonly commandsByChat = signal<Record<string, import('../core/api/types').AvailableCommand[]>>({});
  readonly modesByChat = signal<Record<string, import('../core/api/types').SessionModes | null>>({});
  readonly usageByChat = signal<Record<string, import('../core/api/types').UsageInfo | null>>({});
  readonly elicitationsByChat = signal<Record<string, import('../core/api/types').ElicitationInfo[]>>({});
  readonly reducersByChat = signal<Record<string, EventReducer>>({});
  readonly loadingChats = signal<ReadonlySet<string>>(new Set());
  readonly connectingChats = signal<ReadonlySet<string>>(new Set());
  readonly connectErrors = signal<ErrorMap>({});
  readonly rejectedConfigByChat = signal<Record<string, string>>({});
  readonly historyLoadingByChat = signal<ReadonlySet<string>>(new Set());
  readonly historyHasOlderByChat = signal<BooleanMap>({});
  readonly historyErrors = signal<ErrorMap>({});
  readonly blockedEnvrcByChat = signal<
    Record<string, { path: string; relative_path?: string; message: string }>
  >({});
  /** Chats that failed because the agent needs authentication first. */
  readonly authRequiredByChat = signal<Record<string, AuthRequiredInfo>>({});

  private readonly inFlightConnections = new Map<string, Promise<Chat>>();
  private readonly inFlightConfigs = new Map<string, Promise<ConfigOption[]>>();
  private readonly inFlightChats = new Map<string, Promise<void>>();
  private readonly inFlightHistory = new Map<string, Promise<void>>();
  private readonly historyCursors = signal<CursorMap>({});
  private readonly historyLoaded = new Set<string>();

  loadChats(projectId: string): Promise<void> {
    const existing = this.inFlightChats.get(projectId);
    if (existing) return existing;

    const promise = (async () => {
      this.setSetValue(this.loadingChats, projectId, true);
      try {
        const chats = await this.api.fetchChats(projectId);
        this.chatsByProject.update((current) => ({
          ...current,
          [projectId]: [...chats].sort(compareChatsByRecency),
        }));
      } catch (error) {
        console.error('Failed to load chats for project', projectId, error);
      } finally {
        this.setSetValue(this.loadingChats, projectId, false);
        this.inFlightChats.delete(projectId);
      }
    })();
    this.inFlightChats.set(projectId, promise);
    return promise;
  }

  findChat(chatId: string): Chat | null {
    for (const chats of Object.values(this.chatsByProject())) {
      const chat = chats.find((candidate) => candidate.id === chatId);
      if (chat) return chat;
    }
    return null;
  }

  async autoConnectChat(chatId: string): Promise<void> {
    const chat = this.findChat(chatId);
    if (!chat) return;
    void this.loadChatHistory(chatId);
    if (!this.configLoadedByChat()[chatId]) {
      await this.loadChatConfig(chatId).catch(() => undefined);
    }
    // Dynamic session state stays queryable after reconnect.
    void this.loadChatCommands(chatId).catch(() => undefined);
    void this.loadChatModes(chatId).catch(() => undefined);
    void this.loadChatUsage(chatId).catch(() => undefined);
    void this.loadElicitations(chatId).catch(() => undefined);
  }

  loadChatHistory(chatId: string): Promise<void> {
    if (this.historyLoaded.has(chatId)) return Promise.resolve();
    return this.requestHistory(chatId, undefined, true);
  }

  loadOlderHistory(chatId: string): Promise<void> {
    if (!this.historyHasOlderByChat()[chatId]) return Promise.resolve();
    const cursor = this.historyCursors()[chatId];
    if (cursor == null) return Promise.resolve();
    return this.requestHistory(chatId, cursor);
  }

  retryHistory(chatId: string): Promise<void> {
    if (this.historyLoaded.has(chatId)) return this.loadOlderHistory(chatId);
    return this.loadChatHistory(chatId);
  }

  loadChatConfig(chatId: string): Promise<ConfigOption[]> {
    const existing = this.inFlightConfigs.get(chatId);
    if (existing) return existing;

    const promise = (async () => {
      this.setSetValue(this.connectingChats, chatId, true);
      this.clearError(chatId);
      try {
        const options = this.normalizeConfigOptions(await this.api.fetchChatConfig(chatId));
        this.setConfig(chatId, options);
        return options;
      } catch (error) {
        this.setError(chatId, this.errorMessage(error, 'Failed to load agent configuration'));
        this.captureAuthRequired(chatId, error);
        if (error instanceof ApiError && error.code?.toLowerCase() === 'saved_config_rejected') {
          const optionId = error.details?.['option_id'] ?? error.details?.['optionId'];
          if (typeof optionId === 'string') {
            this.rejectedConfigByChat.update((current) => ({ ...current, [chatId]: optionId }));
          }
        }
        if (error instanceof ApiError && error.code?.toLowerCase() === 'envrc_blocked') {
          const path = typeof error.details?.['path'] === 'string' ? error.details['path'] : '';
          const relative_path =
            typeof error.details?.['relative_path'] === 'string' ? error.details['relative_path'] : undefined;
          const message = typeof error.details?.['message'] === 'string' ? error.details['message'] : error.message;
          this.blockedEnvrcByChat.update((current) => ({ ...current, [chatId]: { path, relative_path, message } }));
        }
        throw error;
      } finally {
        this.setSetValue(this.connectingChats, chatId, false);
        this.inFlightConfigs.delete(chatId);
      }
    })();
    this.inFlightConfigs.set(chatId, promise);
    return promise;
  }

  async retryConnection(chatId: string): Promise<void> {
    // Connection-configuration changes stop the idle ACP process. Reloading
    // config alone does not apply them; session/resume does.
    await this.connectChat(chatId);
  }

  async authorizeChatEnvironment(
    chatId: string,
    remember = false,
  ): Promise<{ remembered: boolean; projectId: string | null; relativePath: string | null }> {
    // Captured before `clearError` drops the blocked-env detail below.
    const detail = this.blockedEnvrcByChat()[chatId];
    await this.api.authorizeChatEnvironment(chatId, remember);
    this.clearError(chatId);
    await this.connectChat(chatId).catch(() => undefined);
    return {
      remembered: remember,
      projectId: this.findChat(chatId)?.project_id ?? null,
      relativePath: detail?.relative_path ?? null,
    };
  }

  connectChat(chatId: string): Promise<Chat> {
    const existing = this.inFlightConnections.get(chatId);
    if (existing) return existing;

    const promise = (async () => {
      this.setSetValue(this.connectingChats, chatId, true);
      this.clearError(chatId);
      try {
        let updated: Chat;
        try {
          updated = await this.api.resumeChat(chatId);
        } catch (error) {
          const currentChat = this.findChat(chatId);
          if (!currentChat || currentChat.process_state !== 'RUNNING') {
            this.setError(chatId, this.errorMessage(error, 'Failed to connect to agent'));
            this.captureAuthRequired(chatId, error);
            if (error instanceof ApiError && error.code === 'saved_config_rejected') {
              const optionId = error.details?.['option_id'];
              if (typeof optionId === 'string') {
                this.rejectedConfigByChat.update((current) => ({ ...current, [chatId]: optionId }));
              }
            }
            if (error instanceof ApiError && error.code?.toLowerCase() === 'envrc_blocked') {
              const path = typeof error.details?.['path'] === 'string' ? error.details['path'] : '';
              const relative_path =
                typeof error.details?.['relative_path'] === 'string' ? error.details['relative_path'] : undefined;
              const message = typeof error.details?.['message'] === 'string' ? error.details['message'] : error.message;
              this.blockedEnvrcByChat.update((current) => ({ ...current, [chatId]: { path, relative_path, message } }));
            }
          }
          throw error;
        }

        this.applyChatPatch(chatId, updated);
        this.clearError(chatId);
        this.clearAuthRequired(chatId);
        try {
          await this.fetchConfig(chatId);
        } catch (error) {
          this.setError(chatId, this.errorMessage(error, 'Failed to load agent configuration'));
          throw error;
        }
        // Dynamic state stays queryable after reconnect; failures leave
        // prior snapshots in place.
        await Promise.allSettled([
          this.loadChatCommands(chatId),
          this.loadChatModes(chatId),
          this.loadChatUsage(chatId),
          this.loadElicitations(chatId),
        ]);
        return updated;
      } finally {
        this.setSetValue(this.connectingChats, chatId, false);
        this.inFlightConnections.delete(chatId);
      }
    })();
    this.inFlightConnections.set(chatId, promise);
    return promise;
  }

  async fetchConfig(chatId: string): Promise<ConfigOption[]> {
    const options = this.normalizeConfigOptions(await this.api.fetchChatConfig(chatId));
    this.setConfig(chatId, options);
    return options;
  }

  async loadChatCommands(chatId: string): Promise<void> {
    try {
      const commands = await this.api.fetchChatCommands(chatId);
      this.commandsByChat.update((c) => ({ ...c, [chatId]: commands ?? [] }));
    } catch {
      // Old agents omit commands; an empty list keeps the composer working.
    }
  }

  async loadChatModes(chatId: string): Promise<void> {
    try {
      const modes = this.normalizeModes(await this.api.fetchChatModes(chatId));
      this.modesByChat.update((c) => ({ ...c, [chatId]: modes ?? null }));
    } catch {
      this.modesByChat.update((c) => ({ ...c, [chatId]: null }));
    }
  }

  async setChatMode(chatId: string, modeId: string): Promise<void> {
    const modes = this.normalizeModes(await this.api.setChatMode(chatId, modeId));
    this.modesByChat.update((c) => ({ ...c, [chatId]: modes }));
  }

  private normalizeModes(value: unknown): import('../core/api/types').SessionModes | null {
    if (!value || typeof value !== 'object') return null;
    const obj = value as Record<string, unknown>;
    const current =
      typeof obj['current_mode_id'] === 'string'
        ? (obj['current_mode_id'] as string)
        : typeof obj['currentModeId'] === 'string'
          ? (obj['currentModeId'] as string)
          : null;
    const rawModes = Array.isArray(obj['available_modes'])
      ? (obj['available_modes'] as unknown[])
      : Array.isArray(obj['availableModes'])
        ? (obj['availableModes'] as unknown[])
        : null;
    if (!current || !rawModes) return null;
    return {
      current_mode_id: current,
      available_modes: rawModes.map((m) => {
        const entry = (m ?? {}) as Record<string, unknown>;
        return {
          id: String(entry['id'] ?? ''),
          name: String(entry['name'] ?? entry['id'] ?? ''),
          description: typeof entry['description'] === 'string' ? (entry['description'] as string) : null,
        };
      }),
    };
  }

  async loadChatUsage(chatId: string): Promise<void> {
    try {
      const usage = this.normalizeUsage(await this.api.fetchChatUsage(chatId));
      this.usageByChat.update((c) => ({ ...c, [chatId]: usage ?? null }));
    } catch {
      // Usage is optional; absence leaves the indicator hidden.
    }
  }

  private normalizeUsage(value: unknown): import('../core/api/types').UsageInfo | null {
    if (!value || typeof value !== 'object') return null;
    const obj = value as Record<string, unknown>;
    const used = Number(obj['used']);
    const size = Number(obj['size']);
    if (!Number.isFinite(used) || !Number.isFinite(size)) return null;
    let amount: number | null = null;
    let currency: string | null = null;
    if (typeof obj['cost_amount'] === 'number') amount = obj['cost_amount'] as number;
    else if (obj['cost'] && typeof (obj['cost'] as Record<string, unknown>)['amount'] === 'number') {
      amount = (obj['cost'] as Record<string, unknown>)['amount'] as number;
    }
    if (typeof obj['cost_currency'] === 'string') currency = obj['cost_currency'] as string;
    else if (obj['cost'] && typeof (obj['cost'] as Record<string, unknown>)['currency'] === 'string') {
      currency = (obj['cost'] as Record<string, unknown>)['currency'] as string;
    }
    return { used, size, cost_amount: amount, cost_currency: currency };
  }

  async loadElicitations(chatId: string): Promise<void> {
    try {
      const list = await this.api.fetchElicitations(chatId);
      this.elicitationsByChat.update((c) => ({ ...c, [chatId]: list ?? [] }));
    } catch {
      this.elicitationsByChat.update((c) => ({ ...c, [chatId]: [] }));
    }
  }

  async respondElicitation(chatId: string, id: string, action: string, content?: unknown): Promise<void> {
    await this.api.respondElicitation(chatId, id, action, content);
    // The elicitation_response event will mark the entry; optimistically drop
    // it from the pending list so the UI feels immediate.
    this.elicitationsByChat.update((c) => ({
      ...c,
      [chatId]: (c[chatId] ?? []).filter((e) => e.id !== id),
    }));
  }

  async deleteRemoteSession(chatId: string, remoteId: string): Promise<void> {
    await this.api.deleteRemoteSession(chatId, remoteId);
  }

  async sendPrompt(chatId: string, text: string | import('../core/api/types').RichContentBlock[]): Promise<void> {
    try {
      await this.api.promptChat(chatId, text);
      this.clearError(chatId);
      this.clearAuthRequired(chatId);
      this.applyChatPatch(chatId, { turn_state: 'PROMPTING' });
    } catch (error) {
      this.captureAuthRequired(chatId, error);
      if (error instanceof ApiError && error.code?.toLowerCase() === 'saved_config_rejected') {
        const optionId = error.details?.['option_id'] ?? error.details?.['optionId'];
        if (typeof optionId === 'string') {
          this.rejectedConfigByChat.update((current) => ({ ...current, [chatId]: optionId }));
        }
      }
      if (error instanceof ApiError && error.code?.toLowerCase() === 'envrc_blocked') {
        const path = typeof error.details?.['path'] === 'string' ? error.details['path'] : '';
        const relative_path =
          typeof error.details?.['relative_path'] === 'string' ? error.details['relative_path'] : undefined;
        const message = typeof error.details?.['message'] === 'string' ? error.details['message'] : error.message;
        this.blockedEnvrcByChat.update((current) => ({ ...current, [chatId]: { path, relative_path, message } }));
      }
      this.setError(chatId, this.errorMessage(error, 'Failed to send prompt'));
      throw error;
    }
  }

  async cancelActiveTurn(chatId: string): Promise<void> {
    await this.api.cancelChat(chatId);
    this.applyChatPatch(chatId, { turn_state: 'CANCELLING' });
  }

  async stopChatProcess(chatId: string): Promise<void> {
    await this.api.stopChat(chatId);
    this.applyChatPatch(chatId, { process_state: 'STOPPED', turn_state: 'IDLE' });
  }

  async renameChat(chatId: string, title: string): Promise<void> {
    this.applyChatPatch(chatId, await this.api.editChat(chatId, { title }));
  }

  async archiveChat(chatId: string, archived: boolean): Promise<void> {
    this.applyChatPatch(chatId, await this.api.editChat(chatId, { archived }));
  }

  async deleteChat(chatId: string): Promise<Chat | null> {
    const chat = this.findChat(chatId);
    await this.api.deleteChat(chatId);
    this.chatsByProject.update((current) => {
      const next = { ...current };
      for (const [projectId, chats] of Object.entries(next)) {
        next[projectId] = chats.filter((candidate) => candidate.id !== chatId);
      }
      return next;
    });
    this.removeChatState(chatId);
    return chat;
  }

  async setChatConfig(chatId: string, optionId: string, value: unknown): Promise<void> {
    const options = this.normalizeConfigOptions(await this.api.setChatConfig(chatId, optionId, value));
    this.setConfig(chatId, options);
  }

  async resetRejectedConfig(chatId: string): Promise<void> {
    const optionId = this.rejectedConfigByChat()[chatId];
    if (!optionId) return;
    await this.api.clearSavedConfig(chatId, optionId);
    this.rejectedConfigByChat.update((current) => {
      const next = { ...current };
      delete next[chatId];
      return next;
    });
    await this.retryConnection(chatId);
  }

  async createChat(projectId: string, agent: string, title?: string, workspace?: ChatWorkspaceSelection): Promise<Chat> {
    const created = await this.api.createChat(projectId, agent, title, workspace);
    this.chatsByProject.update((current) => ({
      ...current,
      [projectId]: [...(current[projectId] ?? []), created].sort(compareChatsByRecency),
    }));
    return created;
  }

  async respondPermission(chatId: string, requestId: string, optionId: string): Promise<void> {
    await this.api.respondPermission(chatId, requestId, optionId);
    const reducer = this.reducersByChat()[chatId];
    if (reducer?.resolvePermission(requestId, optionId)) {
      this.reducersByChat.set({ ...this.reducersByChat(), [chatId]: reducer });
    }
  }

  removeProject(projectId: string): void {
    this.chatsByProject.update((current) => {
      const next = { ...current };
      delete next[projectId];
      return next;
    });
  }

  handleIncomingEvent(event: SessionEvent): void {
    const { session_id: sessionId, payload } = event;
    if (payload.type === 'config_options' && sessionId) {
      this.setConfig(sessionId, this.normalizeConfigOptions(payload.options));
      return;
    }
    if (payload.type === 'available_commands' && sessionId) {
      const commands = Array.isArray(payload['commands'])
        ? (payload['commands'] as import('../core/api/types').AvailableCommand[])
        : [];
      this.commandsByChat.update((c) => ({ ...c, [sessionId]: commands }));
      return;
    }
    if (payload.type === 'session_modes' && sessionId) {
      const state = this.normalizeModes(payload['state'] ?? null);
      this.modesByChat.update((c) => ({ ...c, [sessionId]: state }));
      return;
    }
    if (payload.type === 'usage_update' && sessionId) {
      const usage = this.normalizeUsage({
        used: payload['used'],
        size: payload['size'],
        cost_amount: payload['cost_amount'],
        cost_currency: payload['cost_currency'],
        cost: payload['cost'],
      });
      if (usage) this.usageByChat.update((c) => ({ ...c, [sessionId]: usage }));
      return;
    }
    if (payload.type === 'elicitation_request' && sessionId) {
      // Keep the queryable pending list in sync for reconnect.
      void this.loadElicitations(sessionId).catch(() => undefined);
    }
    if ((payload.type === 'elicitation_response' || payload.type === 'elicitation_complete') && sessionId) {
      void this.loadElicitations(sessionId).catch(() => undefined);
    }

    if (!sessionId) return;
    const reducers = { ...this.reducersByChat() };
    const reducer = reducers[sessionId] ?? new EventReducer();
    reducer.ingest(event);
    reducers[sessionId] = reducer;
    this.reducersByChat.set(reducers);

    if (payload.type === 'user_message') {
      this.applyChatActivity(sessionId, event.timestamp);
      this.applyChatPatch(sessionId, { turn_started_at: event.timestamp });
    }

    if (payload.type === 'turn_complete') {
      this.applyChatPatch(sessionId, { turn_started_at: null });
    }

    if (payload.type === 'state_change') {
      const process = this.processState(payload.process);
      const turn = this.turnState(payload.turn);
      this.applyChatPatch(sessionId, {
        ...(process ? { process_state: process } : {}),
        ...(turn ? { turn_state: turn } : {}),
      });
    }
  }

  resetEventHistory(): void {
    this.reducersByChat.set({});
  }

  /** Central user-facing activity for one chat. Never reads process_state. */
  chatActivity(chatId: string): ChatActivity {
    const chat = this.findChat(chatId);
    const items = this.reducersByChat()[chatId]?.items() ?? [];
    return deriveChatActivity({
      turnState: chat?.turn_state,
      connectError: this.connectErrors()[chatId],
      rejectedConfig: this.rejectedConfigByChat()[chatId],
      items,
      activeTasks: chat?.active_tasks,
    });
  }

  chatTurnStartedAt(chatId: string): string | null {
    const chat = this.findChat(chatId);
    if (chat && Object.prototype.hasOwnProperty.call(chat, 'turn_started_at')) {
      return chat.turn_started_at ?? null;
    }
    return this.reducersByChat()[chatId]?.turnStartedAt() ?? null;
  }

  private setConfig(chatId: string, options: ConfigOption[]): void {
    this.configOptionsByChat.update((current) => ({ ...current, [chatId]: options }));
    this.configLoadedByChat.update((current) => ({ ...current, [chatId]: true }));
  }

  private applyChatPatch(chatId: string, patch: Partial<Chat>): void {
    this.chatsByProject.update((current) => {
      const next = { ...current };
      for (const [projectId, chats] of Object.entries(next)) {
        next[projectId] = chats
          .map((chat) => (chat.id === chatId ? { ...chat, ...patch } : chat))
          .sort(compareChatsByRecency);
      }
      return next;
    });
  }

  private applyChatActivity(chatId: string, updatedAt: string): void {
    this.chatsByProject.update((current) => {
      const next = { ...current };
      for (const [projectId, chats] of Object.entries(next)) {
        next[projectId] = chats
          .map((chat) => {
            if (chat.id !== chatId) return chat;
            const currentTime = Date.parse(chat.updated_at);
            const activityTime = Date.parse(updatedAt);
            return !Number.isFinite(currentTime) || activityTime >= currentTime
              ? { ...chat, updated_at: updatedAt }
              : chat;
          })
          .sort(compareChatsByRecency);
      }
      return next;
    });
  }

  private removeChatState(chatId: string): void {
    this.historyLoaded.delete(chatId);
    for (const target of [
      this.reducersByChat,
      this.configOptionsByChat,
      this.configLoadedByChat,
      this.commandsByChat,
      this.modesByChat,
      this.usageByChat,
      this.elicitationsByChat,
      this.connectErrors,
      this.rejectedConfigByChat,
      this.blockedEnvrcByChat,
      this.authRequiredByChat,
      this.historyHasOlderByChat,
      this.historyErrors,
      this.historyCursors,
    ] as WritableSignal<Record<string, unknown>>[]) {
      target.update((current) => {
        const next = { ...current };
        delete next[chatId];
        return next;
      });
    }
  }

  private requestHistory(
    chatId: string,
    beforeSeq: number | undefined,
    establishBaseline = false,
  ): Promise<void> {
    const existing = this.inFlightHistory.get(chatId);
    if (existing) return existing;

    const promise = (async () => {
      this.setSetValue(this.historyLoadingByChat, chatId, true);
      this.historyErrors.update((current) => {
        if (!(chatId in current)) return current;
        const next = { ...current };
        delete next[chatId];
        return next;
      });
      try {
        const throughSeq = establishBaseline ? await this.socket.waitForBaseline() : undefined;
        const page = establishBaseline
          ? await this.api.fetchChatHistory(chatId, beforeSeq, throughSeq)
          : await this.api.fetchChatHistory(chatId, beforeSeq);
        const reducers = { ...this.reducersByChat() };
        const reducer = reducers[chatId] ?? new EventReducer();
        for (const event of page.events) reducer.ingest(event);
        reducers[chatId] = reducer;
        this.reducersByChat.set(reducers);
        this.historyLoaded.add(chatId);
        this.historyHasOlderByChat.update((current) => ({
          ...current,
          [chatId]: page.has_older,
        }));
        this.historyCursors.update((current) => ({
          ...current,
          [chatId]: page.next_cursor,
        }));
      } catch (error) {
        this.historyErrors.update((current) => ({
          ...current,
          [chatId]: this.errorMessage(error, 'Failed to load chat history'),
        }));
      } finally {
        this.setSetValue(this.historyLoadingByChat, chatId, false);
        this.inFlightHistory.delete(chatId);
      }
    })();
    this.inFlightHistory.set(chatId, promise);
    return promise;
  }

  private setError(chatId: string, message: string): void {
    this.connectErrors.update((current) => ({ ...current, [chatId]: message }));
  }

  /**
   * Records a structured `auth_required` failure. The chat stays intact, so
   * the view can link to the agent's authentication surface and retry after.
   */
  private captureAuthRequired(chatId: string, error: unknown): void {
    if (!(error instanceof ApiError) || error.code?.toLowerCase() !== 'auth_required') return;
    const details = error.details ?? {};
    const agentId = details['agent_id'] ?? details['agentId'];
    const info: AuthRequiredInfo = {
      agent_id: typeof agentId === 'string' ? agentId : this.findChat(chatId)?.agent,
      agent_name: typeof details['agent_name'] === 'string' ? (details['agent_name'] as string) : null,
      message: typeof details['message'] === 'string' ? (details['message'] as string) : error.message,
    };
    this.authRequiredByChat.update((current) => ({ ...current, [chatId]: info }));
  }

  clearAuthRequired(chatId: string): void {
    this.authRequiredByChat.update((current) => {
      if (!(chatId in current)) return current;
      const next = { ...current };
      delete next[chatId];
      return next;
    });
  }

  private clearError(chatId: string): void {
    this.connectErrors.update((current) => {
      if (!(chatId in current)) return current;
      const next = { ...current };
      delete next[chatId];
      return next;
    });
    this.blockedEnvrcByChat.update((current) => {
      if (!(chatId in current)) return current;
      const next = { ...current };
      delete next[chatId];
      return next;
    });
  }

  private setSetValue(target: WritableSignal<ReadonlySet<string>>, value: string, present: boolean): void {
    target.update((current) => {
      const next = new Set(current);
      if (present) next.add(value);
      else next.delete(value);
      return next;
    });
  }

  private normalizeConfigOptions(value: unknown): ConfigOption[] {
    if (!Array.isArray(value)) return [];
    return value.map((raw) => {
      const option = (raw ?? {}) as Record<string, unknown>;
      const rawOptions = Array.isArray(option['options']) ? option['options'] : undefined;
      const options = rawOptions?.map((item) => {
        const entry = (item ?? {}) as Record<string, unknown>;
        if (Array.isArray(entry['options'])) {
          return {
            group: String(entry['group'] ?? entry['name'] ?? ''),
            options: (entry['options'] as unknown[]).map((child) => {
              const valueEntry = (child ?? {}) as Record<string, unknown>;
              return {
                value: valueEntry['value'],
                name: String(valueEntry['name'] ?? valueEntry['label'] ?? valueEntry['value'] ?? ''),
                description:
                  typeof valueEntry['description'] === 'string'
                    ? (valueEntry['description'] as string)
                    : undefined,
              };
            }),
          };
        }
        return {
          value: entry['value'],
          name: String(entry['name'] ?? entry['label'] ?? entry['value'] ?? ''),
          description:
            typeof entry['description'] === 'string' ? (entry['description'] as string) : undefined,
        };
      });
      return {
        id: String(option['id'] ?? ''),
        name: String(option['name'] ?? option['label'] ?? option['id'] ?? ''),
        type: String(option['type'] ?? ''),
        currentValue: option['currentValue'] ?? option['current_value'],
        description: typeof option['description'] === 'string' ? option['description'] : undefined,
        category: typeof option['category'] === 'string' ? option['category'] : undefined,
        options,
      };
    });
  }

  private processState(value: unknown): ProcessState | undefined {
    return ['STARTING', 'RUNNING', 'STOPPED', 'DEAD'].includes(String(value))
      ? (String(value) as ProcessState)
      : undefined;
  }

  private turnState(value: unknown): TurnState | undefined {
    return ['IDLE', 'PROMPTING', 'CANCELLING'].includes(String(value))
      ? (String(value) as TurnState)
      : undefined;
  }

  private errorMessage(error: unknown, fallback: string): string {
    return error instanceof Error && error.message ? error.message : fallback;
  }
}
