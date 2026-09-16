import { Component, computed, input, output } from '@angular/core';
import { MatButtonModule } from '@angular/material/button';
import { MatIconModule } from '@angular/material/icon';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import { MatTooltipModule } from '@angular/material/tooltip';
import type {
  AgentAuthFlow,
  AgentAuthMethod,
  AgentAuthState,
  AgentMutability,
  AgentSummary,
  ObservedAuthState,
  ProtocolAuthElicitation,
  ProtocolAuthFlow,
} from '../../core/api/types';

@Component({
  selector: 'hub-agent-card',
  imports: [
    MatButtonModule,
    MatIconModule,
    MatProgressSpinnerModule,
    MatTooltipModule,
  ],
  templateUrl: './agent-card.html',
  styleUrl: './agent-card.scss',
})
export class AgentCardComponent {
  readonly agent = input.required<AgentSummary>();
  readonly auth = input<AgentAuthState | null>(null);
  readonly authLoading = input(false);
  readonly authError = input<string | null>(null);
  readonly protocolFlow = input<ProtocolAuthFlow | null>(null);
  readonly protocolElicitations = input<ProtocolAuthElicitation[]>([]);
  readonly protocolLoading = input(false);
  readonly terminalFlow = input<AgentAuthFlow | null>(null);

  readonly authenticate = output<string>();
  readonly terminal = output<string>();
  readonly logout = output<void>();
  readonly clearCredentials = output<void>();
  readonly cancelProtocol = output<void>();
  readonly dismissProtocol = output<void>();
  readonly respondElicitation = output<{ id: string; action: string }>();
  readonly resumeTerminal = output<string>();
  readonly cancelTerminal = output<void>();
  readonly edit = output<void>();
  readonly remove = output<void>();
  readonly update = output<void>();
  readonly uninstall = output<void>();
  readonly environment = output<void>();
  readonly retryAuth = output<void>();

  readonly mutability = computed<AgentMutability>(() => {
    const agent = this.agent();
    if (agent.mutability) return agent.mutability;
    if (agent.source === 'batey_managed') return 'editable';
    if (agent.source === 'registry') return 'registry_managed';
    return 'read_only';
  });

  readonly targeted = input(false);
  readonly available = computed(() => this.agent().availability === 'available');
  readonly observed = computed<ObservedAuthState>(() => this.auth()?.observed_state ?? 'unknown');
  readonly isAuthenticated = computed(() => this.observed() === 'authenticated');
  readonly logoutSupported = computed(() => this.auth()?.logout_supported === true);
  /** This agent has never been explicitly checked: there is no cache entry
   * to show, so the card offers a check instead of guessing at methods.
   * Discovery-only: whether sign-in evidence exists is a separate question,
   * shown through `observed_freshness`/`authStatusLabel` regardless. */
  readonly neverChecked = computed(() => (this.auth()?.freshness ?? 'unknown') === 'unknown');
  /** Whether the observed sign-in evidence itself (not the method list) has
   * aged past trust or was invalidated. A bare method-list check never
   * freshens this, and recording new evidence never freshens the method
   * list, so this is judged on its own timestamp. Authenticated-but-stale
   * is never shown as timeless truth. */
  readonly isStale = computed(() => this.auth()?.observed_freshness === 'stale');
  readonly canManageEnv = computed(() => {
    const mutability = this.mutability();
    return mutability === 'editable' || mutability === 'registry_managed';
  });
  /** Normal Log out only when Batey observed authenticated state. */
  readonly showLogout = computed(() => this.logoutSupported() && this.isAuthenticated());
  /** Lower-emphasis credential clear only when auth was observed required. */
  readonly showClear = computed(() => this.logoutSupported() && this.observed() === 'authentication_required');
  /**
   * Sign-in methods are hidden once Batey observed authenticated state, so a
   * signed-in agent never shows login choices beside the ordinary Log out.
   * `unknown` shows methods normally because it is an absence of evidence.
   */
  readonly showMethods = computed(() => !this.isAuthenticated());

  /** A user-visible status label, or null for the internal `unknown` state.
   * Stale evidence is never shown as if it were freshly verified. */
  readonly authStatusLabel = computed<string | null>(() => {
    switch (this.observed()) {
      case 'authenticated':
        return this.isStale() ? 'Previously signed in' : 'Authenticated';
      case 'authentication_required':
        return 'Authentication required';
      default:
        return null;
    }
  });

  elicitationHost(url: string | null | undefined): string {
    if (!url) return '';
    try {
      return new URL(url).host;
    } catch {
      return '';
    }
  }

  sourceLabel(source: string): string {
    switch (source) {
      case 'batey_managed':
        return 'Custom';
      case 'registry':
        return 'Registry';
      case 'builtin':
        return 'Built-in';
      case 'file':
        return 'File';
      case 'declarative':
        return 'Declarative';
      default:
        return source;
    }
  }

  methodTypeLabel(method: AgentAuthMethod): string {
    switch (method.type) {
      case 'agent':
        return method.supported ? 'In-app' : 'In-app (unsupported)';
      case 'terminal':
        return method.supported ? 'Terminal' : 'Terminal (unsupported)';
      default:
        return `Unsupported (${method.type})`;
    }
  }
}
