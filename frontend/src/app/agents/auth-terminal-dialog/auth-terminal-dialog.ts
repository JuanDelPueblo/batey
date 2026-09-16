import {
  AfterViewInit,
  Component,
  ElementRef,
  OnDestroy,
  inject,
  signal,
  viewChild,
} from '@angular/core';
import { MatButtonModule } from '@angular/material/button';
import { MAT_DIALOG_DATA, MatDialogModule, MatDialogRef } from '@angular/material/dialog';
import { MatIconModule } from '@angular/material/icon';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import { FitAddon } from '@xterm/addon-fit';
import { Terminal } from '@xterm/xterm';
import { ApiService } from '../../core/api/api.service';
import type {
  AgentAuthFlow,
  AgentAuthFlowState,
  AgentAuthMethod,
  AgentAuthSocketIncoming,
  AgentAuthSocketOutgoing,
} from '../../core/api/types';
import { AppStateService } from '../../state/app-state.service';

export interface AuthTerminalDialogData {
  flow: AgentAuthFlow;
  method: AgentAuthMethod;
  /** True when the dialog reconnected to an already-running flow. */
  resumed?: boolean;
}

@Component({
  selector: 'hub-auth-terminal-dialog',
  imports: [
    MatButtonModule,
    MatDialogModule,
    MatIconModule,
    MatProgressSpinnerModule,
  ],
  templateUrl: './auth-terminal-dialog.html',
  styleUrl: './auth-terminal-dialog.scss',
})
export class AuthTerminalDialogComponent implements AfterViewInit, OnDestroy {
  readonly dialogRef = inject(MatDialogRef<AuthTerminalDialogComponent>);
  readonly data = inject<AuthTerminalDialogData>(MAT_DIALOG_DATA);
  readonly flow = this.data.flow;
  readonly method = this.data.method;
  private readonly api = inject(ApiService);
  private readonly state = inject(AppStateService);

  readonly flowState = signal<AgentAuthFlowState>(this.flow.state);
  readonly exitCode = signal<number | null>(this.flow.exit_code ?? null);
  readonly reason = signal<string | null>(this.flow.reason ?? null);
  readonly errorMessage = signal('');
  readonly connected = signal(false);

  private readonly terminalContainer = viewChild<ElementRef<HTMLElement>>('terminal');
  term: Terminal | null = null;
  private fitAddon: FitAddon | null = null;
  private socket: WebSocket | null = null;
  private observer: ResizeObserver | null = null;
  private cancelled = false;

  ngAfterViewInit(): void {
    this.openSocket();
    this.initTerminal();
  }

  ngOnDestroy(): void {
    this.observer?.disconnect();
    this.socket?.close();
    this.term?.dispose();
  }

  get running(): boolean {
    return this.flowState() === 'running';
  }

  focusTerminal(): void {
    this.term?.focus();
  }

  async cancel(): Promise<void> {
    if (!this.running || this.cancelled) {
      this.dialogRef.close();
      return;
    }
    this.cancelled = true;
    try {
      await this.api.cancelAgentAuthFlow(this.flow.flow_id);
    } catch {
      // The dialog still closes; the flow ends with the socket.
    }
    this.send({ type: 'input', data: '\u0003' });
    this.dialogRef.close();
  }

  private initTerminal(): void {
    const element = this.terminalContainer()?.nativeElement;
    if (!element) return;

    this.term = new Terminal({
      cursorBlink: true,
      fontSize: 13,
      fontFamily: "'Roboto Mono', ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
      theme: {
        background: '#14151a',
        foreground: '#d7dae0',
        cursor: '#ffffff',
      },
      convertEol: true,
    });

    this.fitAddon = new FitAddon();
    this.term.loadAddon(this.fitAddon);

    try {
      this.term.open(element);
      this.fitAddon.fit();
    } catch {
      // Fit or open may fail in mock/test environments
    }

    this.term.onData((data) => {
      if (this.running) {
        this.send({ type: 'input', data });
      }
    });

    this.term.onResize(({ cols, rows }) => {
      this.send({ type: 'resize', cols, rows });
    });

    if (typeof ResizeObserver !== 'undefined') {
      this.observer = new ResizeObserver(() => {
        try {
          this.fitAddon?.fit();
        } catch {
          // ignore
        }
      });
      this.observer.observe(element);
    }

    this.sendResize();
    this.focusTerminal();
  }

  private openSocket(): void {
    try {
      this.socket = new WebSocket(this.api.agentAuthSocketUrl(this.flow.flow_id));
    } catch {
      this.errorMessage.set('Could not open the authentication terminal.');
      this.flowState.set('failed');
      return;
    }
    this.socket.onopen = () => {
      this.connected.set(true);
      this.sendResize();
    };
    this.socket.onmessage = (event) => this.handleMessage(event.data);
    this.socket.onerror = () => {
      this.connected.set(false);
      if (this.running) {
        this.errorMessage.set('The authentication terminal connection failed.');
      }
    };
    this.socket.onclose = () => this.connected.set(false);
  }

  private handleMessage(raw: unknown): void {
    let message: AgentAuthSocketIncoming;
    try {
      message = JSON.parse(String(raw)) as AgentAuthSocketIncoming;
    } catch {
      return;
    }
    if (message.type === 'output') {
      this.term?.write(message.data);
      return;
    }
    if (message.type === 'state') {
      this.flowState.set(message.state);
      this.exitCode.set(message.exit_code ?? null);
      if (message.reason) {
        this.reason.set(message.reason);
      }
      if (message.state === 'succeeded') {
        // Refresh status; never call authenticate again for a terminal method.
        void this.state.loadAgentAuth(this.flow.agent_id).catch(() => undefined);
      }
    }
  }

  sendResize(): void {
    if (this.term && this.term.cols > 0 && this.term.rows > 0) {
      this.send({ type: 'resize', cols: this.term.cols, rows: this.term.rows });
      return;
    }
    const element = this.terminalContainer()?.nativeElement;
    if (!element) return;
    const cols = Math.max(20, Math.floor(element.clientWidth / 8) || 80);
    const rows = Math.max(6, Math.floor(element.clientHeight / 17) || 24);
    this.send({ type: 'resize', cols, rows });
  }

  send(message: AgentAuthSocketOutgoing): void {
    if (this.socket?.readyState !== WebSocket.OPEN) return;
    this.socket.send(JSON.stringify(message));
  }
}
