import { Component, computed, inject, signal } from '@angular/core';
import { DatePipe } from '@angular/common';
import { MatButtonModule } from '@angular/material/button';
import { MatDialog } from '@angular/material/dialog';
import { MatFormFieldModule } from '@angular/material/form-field';
import { MatIconModule } from '@angular/material/icon';
import { MatInputModule } from '@angular/material/input';
import { MatProgressBarModule } from '@angular/material/progress-bar';
import { MatSelectModule } from '@angular/material/select';
import { MatTooltipModule } from '@angular/material/tooltip';
import type { AgentOperation, DistributionKind, RegistryEntry } from '../../core/api/types';
import { AppStateService } from '../../state/app-state.service';
import { ConfirmDialogComponent } from '../confirm-dialog/confirm-dialog';

@Component({
  selector: 'hub-registry-browser',
  imports: [
    DatePipe,
    MatButtonModule,
    MatFormFieldModule,
    MatIconModule,
    MatInputModule,
    MatProgressBarModule,
    MatSelectModule,
    MatTooltipModule,
  ],
  templateUrl: './registry-browser.html',
  styleUrl: './registry-browser.scss',
})
export class RegistryBrowserComponent {
  private readonly state = inject(AppStateService);
  private readonly dialog = inject(MatDialog);

  readonly query = signal('');
  readonly uninstalling = signal<string | null>(null);
  readonly entryErrors = signal<Record<string, string>>({});
  readonly _actionError = signal('');
  readonly actionError = computed(() => this._actionError() || Object.values(this.entryErrors())[0] || '');
  readonly busy = computed(() => {
    const active = Object.keys(this.state.operationsByAgent());
    return active[0] ?? this.uninstalling();
  });
  readonly notice = signal('');
  readonly distributionByEntry = signal<Record<string, DistributionKind>>({});

  readonly catalog = this.state.registry;
  readonly loading = this.state.registryLoading;
  readonly error = computed(() => this.actionError() || this.state.registryError() || this.catalog()?.error || null);

  readonly entries = computed(() => {
    const query = this.query().trim().toLowerCase();
    return (this.catalog()?.agents ?? []).filter((entry) =>
      [entry.name, entry.id, entry.description].some((value) => value.toLowerCase().includes(query)),
    );
  });

  async refresh(): Promise<void> {
    this._actionError.set('');
    this.entryErrors.set({});
    await this.state.refreshRegistry();
  }

  distributionFor(entry: RegistryEntry): DistributionKind | null {
    return this.distributionByEntry()[entry.id] ?? entry.selected_distribution ?? entry.distributions[0] ?? null;
  }

  setDistribution(entry: RegistryEntry, distribution: DistributionKind): void {
    this.distributionByEntry.update((current) => ({ ...current, [entry.id]: distribution }));
  }

  operationFor(entry: RegistryEntry): AgentOperation | null {
    const id = entry.installed_as ?? entry.id;
    return this.state.operationForAgent(id) ?? this.state.operationForAgent(entry.id) ?? null;
  }

  isEntryBusy(entry: RegistryEntry): boolean {
    const op = this.operationFor(entry);
    return (op !== null && op.state === 'running') || this.uninstalling() === entry.id;
  }

  errorFor(entry: RegistryEntry): string | null {
    return this.entryErrors()[entry.id] ?? null;
  }

  clearEntryError(entry: RegistryEntry): void {
    this._actionError.set('');
    this.entryErrors.update((current) => {
      if (!(entry.id in current)) return current;
      const next = { ...current };
      delete next[entry.id];
      return next;
    });
  }

  setEntryError(entry: RegistryEntry, message: string): void {
    this._actionError.set(message);
    this.entryErrors.update((current) => ({ ...current, [entry.id]: message }));
  }

  formatBytes(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    const kb = bytes / 1024;
    if (kb < 1024) return `${kb.toFixed(1)} KB`;
    const mb = kb / 1024;
    return `${mb.toFixed(1)} MB`;
  }

  stageLabel(op: AgentOperation): string {
    switch (op.stage) {
      case 'queued':
      case 'resolving':
        return 'Resolving...';
      case 'downloading': {
        if (op.total_bytes && op.total_bytes > 0) {
          return `Downloading... (${this.formatBytes(op.downloaded_bytes)} / ${this.formatBytes(op.total_bytes)})`;
        }
        if (op.downloaded_bytes > 0) {
          return `Downloading... (${this.formatBytes(op.downloaded_bytes)})`;
        }
        return 'Downloading...';
      }
      case 'verifying':
        return 'Verifying checksum...';
      case 'extracting':
        return 'Extracting archive...';
      case 'preparing':
        return 'Preparing package...';
      case 'finalizing':
        return 'Finalizing...';
      case 'completed':
        return 'Completed';
      case 'failed':
        return 'Failed';
      default:
        return 'Working...';
    }
  }

  isDeterminate(op: AgentOperation): boolean {
    return op.stage === 'downloading' && op.total_bytes !== null && op.total_bytes > 0;
  }

  downloadPercent(op: AgentOperation): number {
    if (!op.total_bytes || op.total_bytes <= 0) return 0;
    return Math.min(100, Math.round((op.downloaded_bytes / op.total_bytes) * 100));
  }

  async install(entry: RegistryEntry): Promise<void> {
    const distribution = this.distributionFor(entry);
    if (!distribution) return;
    this.clearEntryError(entry);
    this.notice.set('');
    try {
      await this.state.installRegistryAgent({
        registry_id: entry.id,
        distribution,
        display_name: entry.name,
      });
      this.notice.set(`Installed ${entry.name}.`);
    } catch (error: unknown) {
      this.setEntryError(entry, this.message(error, `Failed to install ${entry.name}`));
    }
  }

  async update(entry: RegistryEntry): Promise<void> {
    const id = entry.installed_as ?? entry.id;
    this.clearEntryError(entry);
    this.notice.set('');
    try {
      const outcome = await this.state.updateAgent(id);
      this.notice.set(
        outcome?.to_version
          ? `Updated ${entry.name} to v${outcome.to_version}.`
          : `${entry.name} is already at the newest version.`,
      );
    } catch (error: unknown) {
      this.setEntryError(entry, this.message(error, `Failed to update ${entry.name}`));
    }
  }

  async uninstall(entry: RegistryEntry): Promise<void> {
    const id = entry.installed_as ?? entry.id;
    const confirmed = await this.confirm(
      `Uninstall ${entry.name}`,
      'The registry-managed install is removed. Chats that still use it keep their history but cannot start a new session. Any chat with this agent keeps its history.',
      'Uninstall',
    );
    if (!confirmed) return;
    this.uninstalling.set(entry.id);
    this.clearEntryError(entry);
    this.notice.set('');
    try {
      await this.state.removeAgent(id);
      this.notice.set(`Uninstalled ${entry.name}.`);
    } catch (error: unknown) {
      this.setEntryError(entry, this.message(error, `Failed to uninstall ${entry.name}`));
    } finally {
      this.uninstalling.set(null);
    }
  }

  statusLabel(status: string | undefined): string {
    switch (status) {
      case 'fresh':
        return 'Freshly fetched';
      case 'cached':
        return 'Cached catalog';
      case 'unavailable':
        return 'Registry unavailable';
      default:
        return '';
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
