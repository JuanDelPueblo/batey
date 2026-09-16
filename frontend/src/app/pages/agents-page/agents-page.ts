import { Component, OnDestroy, OnInit, inject, signal } from '@angular/core';
import { ActivatedRoute, RouterLink } from '@angular/router';
import { MatButtonModule } from '@angular/material/button';
import { MatDialog } from '@angular/material/dialog';
import { MatIconModule } from '@angular/material/icon';
import { MatProgressBarModule } from '@angular/material/progress-bar';
import type {
  AgentAuthFlow,
  AgentAuthState,
  AgentSummary,
  ProtocolAuthElicitation,
  ProtocolAuthFlow,
  ProtocolAuthInteraction,
} from '../../core/api/types';
import { AgentCardComponent } from '../../agents/agent-card/agent-card';
import { AuthTerminalDialogComponent } from '../../agents/auth-terminal-dialog/auth-terminal-dialog';
import { ConfirmDialogComponent } from '../../agents/confirm-dialog/confirm-dialog';
import { CustomAgentDialogComponent } from '../../agents/custom-agent-dialog/custom-agent-dialog';
import { AppStateService } from '../../state/app-state.service';

@Component({
  selector: 'hub-agents-page',
  imports: [
    AgentCardComponent,
    MatButtonModule,
    MatIconModule,
    MatProgressBarModule,
    RouterLink,
  ],
  templateUrl: './agents-page.html',
  styleUrl: './agents-page.scss',
})
export class AgentsPageComponent implements OnInit, OnDestroy {
  readonly state = inject(AppStateService);
  private readonly dialog = inject(MatDialog);
  private readonly route = inject(ActivatedRoute);

  readonly actionError = signal('');
  readonly notice = signal('');
  readonly targetAgentId = signal<string | null>(null);
  private polling = new Map<string, { cancelled: boolean }>();
  private readonly pollIntervalMs = 1000;
  private readonly pollTimeoutMs = 10 * 60 * 1000;

  ngOnDestroy(): void {
    for (const entry of this.polling.values()) entry.cancelled = true;
    this.polling.clear();
  }

  ngOnInit(): void {
    const initialTarget = this.route.snapshot.queryParamMap.get('agent');
    if (initialTarget) {
      this.targetAgentId.set(initialTarget);
    }
    this.route.queryParamMap.subscribe((params) => {
      const agent = params.get('agent');
      if (agent) {
        this.targetAgentId.set(agent);
        this.targetAgentAuthSection(agent);
      }
    });
    void this.initialize();
  }

  async initialize(): Promise<void> {
    await this.state.loadAgents();
    await this.loadAuthForAll();
    const target = this.targetAgentId();
    if (target) {
      this.targetAgentAuthSection(target);
    }
  }

  authFor(id: string): AgentAuthState | null {
    return this.state.authByAgent()[id] ?? null;
  }

  authLoadingFor(id: string): boolean {
    return this.state.authLoading().has(id);
  }

  authErrorFor(id: string): string | null {
    return this.state.authErrors()[id] ?? null;
  }

  protocolFlowFor(id: string): ProtocolAuthFlow | null {
    return this.state.protocolFlowsByAgent()[id] ?? null;
  }

  terminalFlowFor(id: string): AgentAuthFlow | null {
    return this.state.terminalFlowsByAgent()[id] ?? null;
  }

  protocolElicitationsFor(flowId: string): ProtocolAuthElicitation[] {
    return this.state.protocolElicitationsByFlow()[flowId] ?? [];
  }

  protocolInteractionFor(flowId: string): ProtocolAuthInteraction | null {
    return this.state.protocolInteractionsByFlow()[flowId] ?? null;
  }

  protocolLoadingFor(id: string): boolean {
    return this.state.protocolLoading().has(id);
  }

  /** The user explicitly asked to check this agent, so this is the one
   * place a card click may start its ACP process. */
  async reloadAuth(agent: AgentSummary): Promise<void> {
    try {
      await this.state.refreshAgentAuth(agent.id);
    } catch {
      // The card shows the error from the store.
    }
  }

  /** Starts an async protocol flow so the card never sticks in Checking sign-in. */
  async authenticate(agent: AgentSummary, methodId: string): Promise<void> {
    this.actionError.set('');
    this.notice.set('');
    this.stopPolling(agent.id);
    try {
      const flow = await this.state.startProtocolAgentAuth(agent.id, methodId);
      this.pollProtocolFlow(agent, flow.flow_id);
    } catch {
      // The store records the method-level error.
    }
  }

  async cancelProtocol(agent: AgentSummary): Promise<void> {
    const flow = this.protocolFlowFor(agent.id);
    if (!flow) return;
    try {
      await this.state.cancelProtocolAgentAuth(agent.id, flow.flow_id);
      await this.state.loadAgentAuth(agent.id).catch(() => undefined);
    } catch {
      // The card shows the flow state.
    } finally {
      this.stopPolling(agent.id);
    }
  }

  dismissProtocol(agent: AgentSummary): void {
    this.stopPolling(agent.id);
    this.state.clearProtocolAgentAuth(agent.id);
  }

  async respondProtocolElicitation(
    agent: AgentSummary,
    event: { id: string; action: string },
  ): Promise<void> {
    const flow = this.protocolFlowFor(agent.id);
    if (!flow) return;
    try {
      await this.state.respondProtocolElicitation(flow.flow_id, event.id, event.action);
      await this.state.refreshProtocolAgentAuth(agent.id, flow.flow_id).catch(() => undefined);
    } catch (error: unknown) {
      this.actionError.set(this.message(error, 'Failed to answer the authentication step'));
    }
  }

  async relayProtocolCallback(agent: AgentSummary, callbackUrl: string): Promise<void> {
    const flow = this.protocolFlowFor(agent.id);
    if (!flow) return;
    try {
      await this.state.relayProtocolAuthCallback(flow.flow_id, callbackUrl);
      this.notice.set('The sign-in callback was sent to the agent.');
    } catch (error: unknown) {
      this.actionError.set(this.message(error, 'The sign-in callback could not be relayed'));
    }
  }

  private pollProtocolFlow(agent: AgentSummary, flowId: string): void {
    const handle = { cancelled: false };
    this.polling.set(agent.id, handle);
    const started = Date.now();
    const tick = async (): Promise<void> => {
      if (handle.cancelled) return;
      if (Date.now() - started > this.pollTimeoutMs) {
        try {
          await this.state.cancelProtocolAgentAuth(agent.id, flowId);
        } catch {
          // Timeout still ends polling.
        }
        this.actionError.set(`Authentication for ${agent.display_name} timed out. Retry or Cancel.`);
        this.stopPolling(agent.id);
        return;
      }
      try {
        const flow = await this.state.refreshProtocolAgentAuth(agent.id, flowId);
        if (flow.state === 'succeeded') {
          const observed = this.authFor(agent.id)?.observed_state;
          if (observed === 'authenticated') {
            this.notice.set(`Authentication completed for ${agent.display_name}.`);
          } else {
            this.notice.set(
              `Authentication finished for ${agent.display_name}. Status is ${observed ?? 'unknown'}.`,
            );
          }
          this.stopPolling(agent.id);
          return;
        }
        if (flow.state === 'failed' || flow.state === 'timed_out' || flow.state === 'cancelled') {
          this.stopPolling(agent.id);
          return;
        }
      } catch {
        this.stopPolling(agent.id);
        return;
      }
      if (!handle.cancelled) {
        setTimeout(() => void tick(), this.pollIntervalMs);
      }
    };
    void tick();
  }

  private stopPolling(agentId: string): void {
    const handle = this.polling.get(agentId);
    if (handle) handle.cancelled = true;
    this.polling.delete(agentId);
  }

  async logout(agent: AgentSummary): Promise<void> {
    this.actionError.set('');
    this.notice.set('');
    try {
      const view = await this.state.logoutAgent(agent.id);
      if (view.observed_state === 'authentication_required') {
        this.notice.set(`Signed out of ${agent.display_name}.`);
      } else {
        this.notice.set(`Logout finished for ${agent.display_name}. Status is ${view.observed_state}.`);
      }
    } catch {
      // The store records the error.
    }
  }

  async clearCredentials(agent: AgentSummary): Promise<void> {
    this.actionError.set('');
    this.notice.set('');
    try {
      await this.state.logoutAgent(agent.id);
      this.notice.set(`Cleared saved sign-in for ${agent.display_name}.`);
    } catch {
      // The store records the error.
    }
  }

  async openTerminalAuth(agent: AgentSummary, methodId: string): Promise<void> {
    this.actionError.set('');
    this.notice.set('');
    try {
      // Reconnect to an existing terminal flow instead of starting another.
      const existing = this.terminalFlowFor(agent.id);
      if (existing && existing.state === 'running') {
        await this.resumeTerminalAuth(agent, existing.flow_id);
        return;
      }
      const flow = await this.state.startTerminalAgentAuth(agent.id, methodId);
      this.state.setTerminalAgentFlow(agent.id, flow);
      this.openTerminalDialog(agent, flow, methodId, false);
    } catch (error: unknown) {
      this.actionError.set(this.message(error, `Failed to start terminal authentication for ${agent.display_name}`));
    }
  }

  /** Reconnects to an existing terminal flow rather than starting a new process. */
  async resumeTerminalAuth(agent: AgentSummary, flowId: string): Promise<void> {
    this.actionError.set('');
    this.notice.set('');
    try {
      const flow = await this.state.fetchTerminalAgentFlow(flowId);
      this.state.setTerminalAgentFlow(agent.id, flow);
      this.openTerminalDialog(agent, flow, flow.method_id, true);
    } catch (error: unknown) {
      this.actionError.set(this.message(error, `Failed to resume terminal authentication for ${agent.display_name}`));
    }
  }

  async cancelTerminalAuth(agent: AgentSummary): Promise<void> {
    this.actionError.set('');
    try {
      const flow = this.terminalFlowFor(agent.id);
      if (flow) {
        await this.state.cancelTerminalAgentAuth(agent.id, flow.flow_id);
      }
    } catch {
      // The card reflects the flow state.
    } finally {
      this.state.setTerminalAgentFlow(agent.id, null);
      await this.state.loadAgentAuth(agent.id).catch(() => undefined);
    }
  }

  private openTerminalDialog(
    agent: AgentSummary,
    flow: AgentAuthFlow,
    methodId: string,
    resumed: boolean,
  ): void {
    const auth = this.authFor(agent.id);
    const method = auth?.methods.find((m) => m.id === methodId) ?? {
      id: methodId,
      name: methodId,
      type: 'terminal',
      supported: true,
    };
    const ref = this.dialog.open(AuthTerminalDialogComponent, {
      data: { flow, method, resumed },
      disableClose: true,
      width: 'min(900px, calc(100vw - 16px))',
      maxWidth: '96vw',
    });
    ref.afterClosed().subscribe(() => void this.refreshTerminalFlowState(agent));
  }

  /** Reconciles the card's terminal-flow state after the dialog closes. */
  private async refreshTerminalFlowState(agent: AgentSummary): Promise<void> {
    const current = this.terminalFlowFor(agent.id);
    if (!current) return;
    try {
      const flow = await this.state.fetchTerminalAgentFlow(current.flow_id);
      if (flow.state === 'running') {
        this.state.setTerminalAgentFlow(agent.id, flow);
      } else {
        this.state.setTerminalAgentFlow(agent.id, null);
      }
    } catch {
      this.state.setTerminalAgentFlow(agent.id, null);
    }
    await this.state.loadAgentAuth(agent.id).catch(() => undefined);
  }

  private targetAgentAuthSection(agentId: string): void {
    queueMicrotask(() => {
      const authSection = document.getElementById(`agent-auth-${agentId}`);
      if (authSection) {
        authSection.scrollIntoView?.({ behavior: 'smooth', block: 'center' });
        authSection.focus?.();
        return;
      }
      const card = document.getElementById(`agent-card-${agentId}`);
      if (card) {
        card.scrollIntoView?.({ behavior: 'smooth', block: 'center' });
        card.focus?.();
      }
    });
  }

  async createCustom(): Promise<void> {
    this.openCustomDialog(null);
  }

  async editCustom(agent: AgentSummary): Promise<void> {
    this.actionError.set('');
    try {
      const detail = await this.state.fetchAgentDetail(agent.id);
      this.openCustomDialog(detail);
    } catch (error: unknown) {
      this.actionError.set(this.message(error, `Failed to load ${agent.display_name}`));
    }
  }

  async editEnvironment(agent: AgentSummary): Promise<void> {
    this.actionError.set('');
    this.notice.set('');
    try {
      const presence = await this.state.loadAgentEnv(agent.id);
      const { AgentEnvDialogComponent } = await import(
        '../../agents/agent-env-dialog/agent-env-dialog'
      );
      const ref = this.dialog.open(AgentEnvDialogComponent, {
        data: { agent, presence },
        width: 'min(720px, calc(100vw - 24px))',
        maxWidth: '96vw',
      });
      ref.afterClosed().subscribe((saved) => {
        if (saved) this.notice.set(`Environment saved for ${agent.display_name}.`);
      });
    } catch (error: unknown) {
      this.actionError.set(this.message(error, `Failed to load environment for ${agent.display_name}`));
    }
  }

  async removeCustom(agent: AgentSummary): Promise<void> {
    const confirmed = await this.confirm(
      `Remove ${agent.display_name}`,
      'The custom definition is deleted. Chats that still use it keep their history but cannot start a new session.',
      'Remove',
    );
    if (!confirmed) return;
    await this.removeAgent(agent, `Removed ${agent.display_name}.`);
  }

  async update(agent: AgentSummary): Promise<void> {
    this.actionError.set('');
    this.notice.set('');
    try {
      const outcome = await this.state.updateAgent(agent.id);
      this.notice.set(
        outcome.updated
          ? `Updated ${agent.display_name} from ${outcome.from_version} to ${outcome.to_version}.`
          : `${agent.display_name} is already at the newest version.`,
      );
    } catch (error: unknown) {
      this.actionError.set(this.message(error, `Failed to update ${agent.display_name}`));
    }
  }

  async uninstall(agent: AgentSummary): Promise<void> {
    const confirmed = await this.confirm(
      `Uninstall ${agent.display_name}`,
      'The registry-managed install is removed. Chats that still use it keep their history but cannot start a new session.',
      'Uninstall',
    );
    if (!confirmed) return;
    await this.removeAgent(agent, `Uninstalled ${agent.display_name}.`);
  }

  private openCustomDialog(detail: import('../../core/api/types').AgentManagementDetail | null): void {
    const ref = this.dialog.open(CustomAgentDialogComponent, {
      data: { detail },
      width: 'min(720px, calc(100vw - 24px))',
      maxWidth: '96vw',
    });
    ref.afterClosed().subscribe((saved) => {
      if (saved) this.notice.set(detail ? 'Custom agent saved.' : 'Custom agent created.');
    });
  }

  private async removeAgent(agent: AgentSummary, successMessage: string): Promise<void> {
    this.actionError.set('');
    this.notice.set('');
    try {
      const outcome = await this.state.removeAgent(agent.id);
      this.notice.set(
        outcome.deleted
          ? successMessage
          : `${agent.display_name} was retired. ${outcome.retained_chats} chat(s) still refer to it, so their history stays readable.`,
      );
    } catch (error: unknown) {
      this.actionError.set(this.message(error, `Failed to remove ${agent.display_name}`));
    }
  }

  private async loadAuthForAll(): Promise<void> {
    await Promise.allSettled(this.state.agents().map((agent) => this.state.loadAgentAuth(agent.id)));
    await this.recoverActiveFlows();
  }

  /**
   * Rediscovers any active authentication flow on page initialization or
   * reload, so a user never returns to an agent stuck behind an
   * "already has an authentication flow running" conflict with no way out.
   * Covers both protocol and terminal flows.
   */
  async recoverActiveFlows(): Promise<void> {
    for (const agent of this.state.agents()) {
      const active = this.authFor(agent.id)?.active_flow;
      if (!active) continue;
      if (active.kind === 'protocol') {
        if (this.protocolFlowFor(agent.id)) continue;
        this.state.setProtocolFlowFromActive(agent.id, active);
        this.pollProtocolFlow(agent, active.flow_id);
        await this.state.refreshProtocolAgentAuth(agent.id, active.flow_id).catch(() => undefined);
      } else if (active.kind === 'terminal') {
        if (this.terminalFlowFor(agent.id)) continue;
        try {
          const flow = await this.state.fetchTerminalAgentFlow(active.flow_id);
          this.state.setTerminalAgentFlow(agent.id, flow);
        } catch {
          // The flow ended between discovery and fetch; the card stays clear.
        }
      }
    }
  }

  private confirm(title: string, message: string, confirmLabel: string): Promise<boolean> {
    return new Promise((resolve) => {
      const ref = this.dialog.open(ConfirmDialogComponent, {
        data: { title, message, confirmLabel, destructive: true },
        width: 'min(520px, calc(100vw - 32px))',
      });
      ref.afterClosed().subscribe((result) => resolve(Boolean(result)));
    });
  }

  private message(error: unknown, fallback: string): string {
    return error instanceof Error && error.message ? error.message : fallback;
  }
}
