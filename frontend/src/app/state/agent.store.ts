import { computed, inject, Service, signal, WritableSignal } from '@angular/core';
import { ApiService } from '../core/api/api.service';
import type {
  ActiveAuthFlow,
  AgentOperation,
  AgentAuthFlow,
  AgentAuthState,
  AgentEnvEdit,
  AgentEnvPresence,
  AgentManagementDetail,
  AgentSummary,
  CustomAgentInput,
  InstallRegistryAgentInput,
  ProtocolAuthElicitation,
  ProtocolAuthFlow,
  ProtocolAuthInteraction,
  RegistryCatalog,
  RemoveOutcome,
  UpdateOutcome,
  ValidationReport,
} from '../core/api/types';
import { ProjectStore } from './project.store';

type ErrorMap = Record<string, string>;

const MAX_OPERATION_POLL_FAILURES = 3;

/**
 * Owns the agent management surface: the installed catalog, the ACP Registry
 * browse cache, editable custom definitions, and authentication state.
 *
 * The installed list is the same signal the new-chat picker reads, so every
 * mutation keeps one authoritative catalog.
 */
@Service()
export class AgentStore {
  private readonly api = inject(ApiService);
  private readonly projectStore = inject(ProjectStore);

  readonly installed = this.projectStore.agents;
  readonly loading = signal(false);
  readonly error = signal<string | null>(null);

  readonly registry = signal<RegistryCatalog | null>(null);
  readonly registryLoading = signal(false);
  readonly registryError = signal<string | null>(null);

  readonly operationsByRegistryId = signal<Record<string, AgentOperation>>({});
  readonly operationsById = signal<Record<string, AgentOperation>>({});
  private readonly activePolls = new Set<string>();

  readonly customDetails = signal<Record<string, AgentManagementDetail>>({});
  readonly envByAgent = signal<Record<string, AgentEnvPresence[]>>({});
  readonly envLoading = signal<ReadonlySet<string>>(new Set());
  readonly envErrors = signal<ErrorMap>({});
  readonly authByAgent = signal<Record<string, AgentAuthState>>({});
  readonly authLoading = signal<ReadonlySet<string>>(new Set());
  readonly authErrors = signal<ErrorMap>({});
  readonly protocolFlowsByAgent = signal<Record<string, ProtocolAuthFlow>>({});
  readonly protocolElicitationsByFlow = signal<Record<string, ProtocolAuthElicitation[]>>({});
  readonly protocolInteractionsByFlow = signal<Record<string, ProtocolAuthInteraction | null>>({});
  readonly protocolLoading = signal<ReadonlySet<string>>(new Set());
  readonly terminalFlowsByAgent = signal<Record<string, AgentAuthFlow>>({});

  readonly available = computed(() =>
    this.installed().filter((agent) => agent.availability === 'available'),
  );

  async loadInstalled(): Promise<void> {
    this.loading.set(true);
    try {
      this.installed.set(await this.api.fetchAgents());
      this.error.set(null);
    } catch (error) {
      this.error.set(this.message(error, 'Failed to load the agent catalog'));
    } finally {
      this.loading.set(false);
    }
  }

  async loadRegistry(refresh = false): Promise<void> {
    this.registryLoading.set(true);
    try {
      this.registry.set(refresh ? await this.api.refreshRegistry() : await this.api.fetchRegistry());
      this.registryError.set(this.registry()?.error ?? null);
    } catch (error) {
      this.registryError.set(this.message(error, 'Failed to load the ACP Registry'));
    } finally {
      this.registryLoading.set(false);
    }
  }

  async refreshRegistry(): Promise<void> {
    await this.loadRegistry(true);
  }

  trackOperation(op: AgentOperation): void {
    this.operationsByRegistryId.update((current) => ({ ...current, [op.registry_id]: op }));
    this.operationsById.update((current) => ({ ...current, [op.id]: op }));
  }

  clearOperation(registryId: string): void {
    this.operationsByRegistryId.update((current) => {
      if (!(registryId in current)) return current;
      const next = { ...current };
      delete next[registryId];
      return next;
    });
  }

  operationFor(registryId: string): AgentOperation | undefined {
    return this.operationsByRegistryId()[registryId];
  }

  async loadOperations(): Promise<void> {
    try {
      const ops = await this.api.fetchAgentOperations();
      for (const op of ops) {
        this.trackOperation(op);
        void this.pollOperationUntilTerminal(op).catch(() => {
          // A server-reported failure is already retained in the operation
          // signal. Startup recovery has no caller to which it can report it.
        });
      }
    } catch {
      // Tolerate background load failure
    }
  }

  async pollOperationUntilTerminal(
    initial: AgentOperation,
    pollIntervalMs = 250,
  ): Promise<AgentOperation> {
    let current = initial;
    this.trackOperation(current);

    if (current.state !== 'running') {
      if (current.state === 'succeeded' || !current.state) {
        await this.loadInstalled();
        await this.loadRegistry();
      } else if (current.state === 'failed') {
        throw new Error(current.error || 'Operation failed');
      }
      return current;
    }

    if (this.activePolls.has(current.id)) {
      return current;
    }
    this.activePolls.add(current.id);

    try {
      let consecutiveFailures = 0;
      while (current.state === 'running') {
        await new Promise((resolve) => setTimeout(resolve, pollIntervalMs));
        try {
          const next = await this.api.fetchAgentOperation(current.id);
          consecutiveFailures = 0;
          current = next;
          this.trackOperation(current);
        } catch {
          consecutiveFailures += 1;
          if (consecutiveFailures >= MAX_OPERATION_POLL_FAILURES) {
            // The operation is still running as far as the server last told us.
            // Keep that snapshot and let a later startup/event recovery resume
            // polling; a status endpoint outage is not an install failure.
            return current;
          }
          // Retry with a small bounded backoff while preserving the last known
          // operation in the UI.
          await new Promise((resolve) => setTimeout(resolve, pollIntervalMs * consecutiveFailures));
        }
      }

      if (current.state === 'succeeded') {
        await this.loadInstalled();
        await this.loadRegistry();
      } else if (current.state === 'failed') {
        throw new Error(current.error || 'Operation failed');
      }
      return current;
    } finally {
      this.activePolls.delete(current.id);
    }
  }

  async installRegistryAgent(input: InstallRegistryAgentInput): Promise<AgentOperation> {
    const op = await this.api.installRegistryAgent(input);
    this.trackOperation(op);
    return this.pollOperationUntilTerminal(op);
  }

  async updateAgent(id: string): Promise<UpdateOutcome> {
    const existing = this.installed().find((a) => a.id === id);
    const fromVersion = existing?.display?.version ?? '0.0.0';
    const op = await this.api.updateRegistryAgent(id);
    this.trackOperation(op);
    const terminal = await this.pollOperationUntilTerminal(op);
    const updatedAgent = this.installed().find((a) => a.id === id);
    const toVersion = updatedAgent?.display?.version ?? fromVersion;
    if (terminal.update_outcome) {
      return { ...terminal.update_outcome, operation: terminal };
    }
    return {
      updated: fromVersion !== toVersion,
      from_version: fromVersion,
      to_version: toVersion,
      agent: updatedAgent,
      operation: terminal,
    };
  }

  async removeAgent(id: string): Promise<RemoveOutcome> {
    const outcome = await this.api.removeAgent(id);
    await this.loadInstalled();
    return outcome;
  }

  async fetchDetail(id: string): Promise<AgentManagementDetail> {
    const detail = await this.api.fetchAgentDetail(id);
    this.customDetails.update((current) => ({ ...current, [id]: detail }));
    return detail;
  }

  async loadAgentEnv(id: string): Promise<AgentEnvPresence[]> {
    this.setSetValue(this.envLoading, id, true);
    try {
      const presence = await this.api.fetchAgentEnv(id);
      // Presence only: values never enter frontend state.
      this.envByAgent.update((current) => ({ ...current, [id]: presence }));
      this.envErrors.update((current) => {
        if (!(id in current)) return current;
        const next = { ...current };
        delete next[id];
        return next;
      });
      return presence;
    } catch (error) {
      this.envErrors.update((current) => ({
        ...current,
        [id]: this.message(error, 'Failed to load agent environment'),
      }));
      throw error;
    } finally {
      this.setSetValue(this.envLoading, id, false);
    }
  }

  async updateAgentEnv(id: string, edits: AgentEnvEdit[]): Promise<AgentEnvPresence[]> {
    this.setSetValue(this.envLoading, id, true);
    try {
      // The request carries values once; the stored response is presence only.
      const presence = await this.api.updateAgentEnv(id, edits);
      this.envByAgent.update((current) => ({ ...current, [id]: presence }));
      this.envErrors.update((current) => {
        if (!(id in current)) return current;
        const next = { ...current };
        delete next[id];
        return next;
      });
      return presence;
    } catch (error) {
      this.envErrors.update((current) => ({
        ...current,
        [id]: this.message(error, 'Failed to save agent environment'),
      }));
      throw error;
    } finally {
      this.setSetValue(this.envLoading, id, false);
    }
  }

  validateCustomAgent(input: CustomAgentInput): Promise<ValidationReport> {
    return this.api.validateCustomAgent(input);
  }

  async createCustomAgent(input: CustomAgentInput): Promise<AgentSummary> {
    const created = await this.api.createCustomAgent(input);
    await this.loadInstalled();
    return created;
  }

  async editCustomAgent(id: string, input: CustomAgentInput): Promise<AgentSummary> {
    const updated = await this.api.editCustomAgent(id, input);
    await this.loadInstalled();
    this.customDetails.update((current) => {
      const next = { ...current };
      delete next[id];
      return next;
    });
    return updated;
  }

  /** A plain cache-only read. Never starts an agent process. */
  async loadAuth(id: string): Promise<AgentAuthState> {
    try {
      const state = await this.api.fetchAgentAuth(id);
      this.authByAgent.update((current) => ({ ...current, [id]: state }));
      this.clearAuthError(id);
      return state;
    } catch (error) {
      this.setAuthError(id, this.message(error, 'Failed to load authentication state'));
      throw error;
    }
  }

  /**
   * Explicit refresh: the only user-triggered action that may start this
   * agent's ACP process just to check its authentication state. A failed
   * probe still updates the card with the last known data instead of
   * throwing, so a stale cache never looks like a broken agent.
   */
  async refreshAuth(id: string): Promise<AgentAuthState> {
    this.setLoading(id, true);
    this.clearAuthError(id);
    try {
      const result = await this.api.refreshAgentAuth(id);
      const { refresh_error, ...state } = result;
      this.authByAgent.update((current) => ({ ...current, [id]: state }));
      if (refresh_error) {
        this.setAuthError(id, refresh_error);
      } else {
        this.clearAuthError(id);
      }
      return state;
    } catch (error) {
      this.setAuthError(id, this.message(error, 'Failed to refresh authentication state'));
      throw error;
    } finally {
      this.setLoading(id, false);
    }
  }

  async authenticate(id: string, methodId: string): Promise<AgentAuthState> {
    this.setLoading(id, true);
    try {
      const state = await this.api.authenticateAgent(id, methodId);
      this.authByAgent.update((current) => ({ ...current, [id]: state }));
      this.clearAuthError(id);
      return state;
    } catch (error) {
      this.setAuthError(id, this.message(error, 'Authentication failed'));
      throw error;
    } finally {
      this.setLoading(id, false);
    }
  }

  async logout(id: string): Promise<AgentAuthState> {
    this.setLoading(id, true);
    try {
      const state = await this.api.logoutAgent(id);
      this.authByAgent.update((current) => ({ ...current, [id]: state }));
      this.clearAuthError(id);
      return state;
    } catch (error) {
      this.setAuthError(id, this.message(error, 'Logout failed'));
      throw error;
    } finally {
      this.setLoading(id, false);
    }
  }

  /** Starts the opaque PTY flow. The browser never chooses command or args. */
  async startTerminalAuth(id: string, methodId: string): Promise<AgentAuthFlow> {
    const flow = await this.api.startTerminalAuth(id, methodId);
    await this.loadAuth(id);
    return flow;
  }

  /** Reconnects to an existing terminal flow instead of starting another. */
  async fetchTerminalFlow(flowId: string): Promise<AgentAuthFlow> {
    return this.api.fetchAgentAuthFlow(flowId);
  }

  setTerminalFlow(agentId: string, flow: AgentAuthFlow | null): void {
    this.terminalFlowsByAgent.update((current) => {
      const next = { ...current };
      if (flow) next[agentId] = flow;
      else delete next[agentId];
      return next;
    });
  }

  async cancelTerminalFlow(agentId: string, flowId: string): Promise<AgentAuthFlow> {
    const flow = await this.api.cancelAgentAuthFlow(flowId);
    this.setTerminalFlow(agentId, flow);
    return flow;
  }

  /** Starts an async protocol flow so a long `authenticate` never blocks the card. */
  async startProtocolAuth(id: string, methodId: string): Promise<ProtocolAuthFlow> {
    const previous = this.protocolFlowsByAgent()[id];
    if (previous) this.clearProtocolFlow(id);
    this.setProtocolLoading(id, true);
    try {
      const flow = await this.api.startProtocolAuth(id, methodId);
      this.protocolFlowsByAgent.update((current) => ({ ...current, [id]: flow }));
      return flow;
    } catch (error) {
      this.setAuthError(id, this.message(error, 'Authentication failed to start'));
      throw error;
    } finally {
      this.setProtocolLoading(id, false);
    }
  }

  async refreshProtocolFlow(agentId: string, flowId: string): Promise<ProtocolAuthFlow> {
    const flow = await this.api.fetchProtocolAuthFlow(flowId);
    this.protocolFlowsByAgent.update((current) => ({ ...current, [agentId]: flow }));
    const [elicitations, interaction] = await Promise.allSettled([
      this.api.fetchProtocolAuthElicitations(flowId),
      this.api.fetchProtocolAuthInteraction(flowId),
    ]);
    if (elicitations.status === 'fulfilled') {
      this.protocolElicitationsByFlow.update((current) => ({
        ...current,
        [flowId]: elicitations.value ?? [],
      }));
    }
    if (interaction.status === 'fulfilled') {
      // A fulfilled null is meaningful: the backend cleared the ephemeral
      // interaction and any stale local URL/draft must disappear.
      this.protocolInteractionsByFlow.update((current) => ({ ...current, [flowId]: interaction.value }));
    }
    if (flow.state === 'succeeded' || flow.state === 'failed' || flow.state === 'cancelled' || flow.state === 'timed_out') {
      this.protocolInteractionsByFlow.update((current) => ({ ...current, [flowId]: null }));
      this.protocolElicitationsByFlow.update((current) => ({ ...current, [flowId]: [] }));
      await this.loadAuth(agentId).catch(() => undefined);
    }
    return flow;
  }

  async cancelProtocolAuth(agentId: string, flowId: string): Promise<ProtocolAuthFlow> {
    const flow = await this.api.cancelProtocolAuthFlow(flowId);
    this.protocolFlowsByAgent.update((current) => ({ ...current, [agentId]: flow }));
    this.protocolInteractionsByFlow.update((current) => ({ ...current, [flowId]: null }));
    this.protocolElicitationsByFlow.update((current) => ({ ...current, [flowId]: [] }));
    return flow;
  }

  clearProtocolFlow(agentId: string): void {
    const flowId = this.protocolFlowsByAgent()[agentId]?.flow_id;
    this.protocolFlowsByAgent.update((current) => {
      if (!(agentId in current)) return current;
      const next = { ...current };
      delete next[agentId];
      return next;
    });
    if (flowId) {
      this.protocolInteractionsByFlow.update((current) => ({ ...current, [flowId]: null }));
      this.protocolElicitationsByFlow.update((current) => ({ ...current, [flowId]: [] }));
    }
  }

  protocolInteractionFor(flowId: string): ProtocolAuthInteraction | null {
    return this.protocolInteractionsByFlow()[flowId] ?? null;
  }

  async relayProtocolCallback(flowId: string, callbackUrl: string): Promise<void> {
    await this.api.relayProtocolAuthCallback(flowId, callbackUrl);
    await this.refreshProtocolFlowForId(flowId);
  }

  private async refreshProtocolFlowForId(flowId: string): Promise<void> {
    const flow = Object.values(this.protocolFlowsByAgent()).find((candidate) => candidate.flow_id === flowId);
    if (!flow) return;
    await this.refreshProtocolFlow(flow.agent_id, flowId);
  }

  /** Seeds a recovered protocol flow from the safe active-flow discovery. */
  setProtocolFlowFromActive(agentId: string, active: ActiveAuthFlow): void {
    const previous = this.protocolFlowsByAgent()[agentId];
    if (previous && previous.flow_id !== active.flow_id) this.clearProtocolFlow(agentId);
    this.protocolFlowsByAgent.update((current) => ({
      ...current,
      [agentId]: {
        flow_id: active.flow_id,
        agent_id: agentId,
        method_id: active.method_id,
        state: active.state as ProtocolAuthFlow['state'],
        reason: null,
        started_at: active.started_at,
        completed_at: null,
      },
    }));
  }

  async respondProtocolElicitation(
    flowId: string,
    elicitationId: string,
    action: string,
    content?: unknown,
  ): Promise<void> {
    await this.api.respondProtocolAuthElicitation(flowId, elicitationId, action, content);
    try {
      const elicitations = await this.api.fetchProtocolAuthElicitations(flowId);
      this.protocolElicitationsByFlow.update((current) => ({
        ...current,
        [flowId]: elicitations ?? [],
      }));
    } catch {
      // The flow poll refreshes the list on its next tick.
    }
  }

  private setLoading(id: string, loading: boolean): void {
    this.setSetValue(this.authLoading, id, loading);
  }

  private setProtocolLoading(id: string, loading: boolean): void {
    this.setSetValue(this.protocolLoading, id, loading);
  }

  private setAuthError(id: string, message: string): void {
    this.authErrors.update((current) => ({ ...current, [id]: message }));
  }

  private clearAuthError(id: string): void {
    this.authErrors.update((current) => {
      if (!(id in current)) return current;
      const next = { ...current };
      delete next[id];
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

  private message(error: unknown, fallback: string): string {
    return error instanceof Error && error.message ? error.message : fallback;
  }
}
