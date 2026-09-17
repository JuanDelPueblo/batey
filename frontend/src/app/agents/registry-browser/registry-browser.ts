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
  readonly cardErrorsByEntry = signal<Record<string, string>>({});
  readonly actionError = signal('');
  readonly notice = signal('');
  readonly distributionByEntry = signal<Record<string, DistributionKind>>({});
  readonly operationsByRegistryId = this.state.operationsByRegistryId ?? signal({});

  readonly busy = computed(() => {
    const ops = this.operationsByRegistryId?.() ?? {};
    const active = Object.values(ops).find((op) => op.state === 'running');
    return active ? active.registry_id : null;
  });

  readonly catalog = this.state.registry;
  readonly loading = this.state.registryLoading;
  readonly error = computed(() => this.actionError() || this.state.registryError() || this.catalog()?.error || null);

  readonly entries = computed(() => {
    const query = this.query().trim().toLowerCase();
    return (this.catalog()?.agents ?? []).filter((entry) =>
      [entry.name, entry.id, entry.description].some((value) => value.toLowerCase().includes(query)),
    );
  });

  operationFor(entry: RegistryEntry): AgentOperation | undefined {
    return this.operationsByRegistryId?.()?.[entry.id];
  }

  isBusy(entry: RegistryEntry): boolean {
    const op = this.operationFor(entry);
    return op?.state === 'running';
  }

  cardError(entryId: string): string | null {
    return this.cardErrorsByEntry()[entryId] ?? null;
  }

  setCardError(entryId: string, message: string): void {
    this.cardErrorsByEntry.update((current) => ({ ...current, [entryId]: message }));
    this.actionError.set(message);
  }

  clearCardError(entryId: string): void {
    this.cardErrorsByEntry.update((current) => {
      if (!(entryId in current)) return current;
      const next = { ...current };
      delete next[entryId];
      return next;
    });
    this.actionError.set('');
  }

  stageLabel(op: AgentOperation): string {
    switch (op.stage) {
      case 'queued':
        return 'Queued...';
      case 'resolving':
        return 'Resolving...';
      case 'downloading':
        if (op.total_bytes && op.total_bytes > 0) {
          const mbDownloaded = (op.bytes_downloaded / (1024 * 1024)).toFixed(1);
          const mbTotal = (op.total_bytes / (1024 * 1024)).toFixed(1);
          return `Downloading (${mbDownloaded} / ${mbTotal} MB)...`;
        }
        return 'Downloading...';
      case 'verifying':
        return 'Verifying...';
      case 'extracting':
        return 'Extracting...';
      case 'preparing':
        return 'Preparing...';
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

  progressMode(op: AgentOperation): 'determinate' | 'indeterminate' {
    return op.total_bytes && op.total_bytes > 0 ? 'determinate' : 'indeterminate';
  }

  progressValue(op: AgentOperation): number {
    if (op.total_bytes && op.total_bytes > 0) {
      return Math.min(100, Math.round((op.bytes_downloaded / op.total_bytes) * 100));
    }
    return 0;
  }

  progressPercent(op: AgentOperation): number {
    return this.progressValue(op);
  }

  async refresh(): Promise<void> {
    this.actionError.set('');
    await this.state.refreshRegistry();
  }

  distributionFor(entry: RegistryEntry): DistributionKind | null {
    return this.distributionByEntry()[entry.id] ?? entry.selected_distribution ?? entry.distributions[0] ?? null;
  }

  setDistribution(entry: RegistryEntry, distribution: DistributionKind): void {
    this.distributionByEntry.update((current) => ({ ...current, [entry.id]: distribution }));
  }

  async install(entry: RegistryEntry): Promise<void> {
    const distribution = this.distributionFor(entry);
    if (!distribution) return;
    this.clearCardError(entry.id);
    this.notice.set('');
    try {
      await this.state.installRegistryAgent({
        registry_id: entry.id,
        distribution,
        display_name: entry.name,
      });
      this.notice.set(`Installed ${entry.name}.`);
      this.clearCardError(entry.id);
      setTimeout(() => this.state.clearOperation(entry.id), 2000);
    } catch (error: unknown) {
      this.state.clearOperation(entry.id);
      this.setCardError(entry.id, this.message(error, `Failed to install ${entry.name}`));
    }
  }

  async update(entry: RegistryEntry): Promise<void> {
    const id = entry.installed_as ?? entry.id;
    this.clearCardError(entry.id);
    this.notice.set('');
    try {
      const outcome = await this.state.updateAgent(id);
      this.notice.set(
        outcome?.updated
          ? `Updated ${entry.name}${outcome.to_version ? ` to v${outcome.to_version}` : ''}.`
          : `${entry.name} is already at the newest version.`,
      );
      this.clearCardError(entry.id);
      setTimeout(() => this.state.clearOperation(entry.id), 2000);
    } catch (error: unknown) {
      this.state.clearOperation(entry.id);
      this.setCardError(entry.id, this.message(error, `Failed to update ${entry.name}`));
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
    this.clearCardError(entry.id);
    this.notice.set('');
    try {
      await this.state.removeAgent(id);
      this.notice.set(`Uninstalled ${entry.name}.`);
    } catch (error: unknown) {
      this.setCardError(entry.id, this.message(error, `Failed to uninstall ${entry.name}`));
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
