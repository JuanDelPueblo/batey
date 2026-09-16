import { Component, inject, signal } from '@angular/core';
import { MatButtonModule } from '@angular/material/button';
import { MAT_DIALOG_DATA, MatDialogModule, MatDialogRef } from '@angular/material/dialog';
import { MatFormFieldModule } from '@angular/material/form-field';
import { MatInputModule } from '@angular/material/input';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import type {
  AgentManagementDetail,
  CustomAgentInput,
  ValidationReport,
} from '../../core/api/types';
import { AppStateService } from '../../state/app-state.service';

export interface CustomAgentDialogData {
  detail: AgentManagementDetail | null;
}

@Component({
  selector: 'hub-custom-agent-dialog',
  imports: [
    MatButtonModule,
    MatDialogModule,
    MatFormFieldModule,
    MatInputModule,
    MatProgressSpinnerModule,
  ],
  templateUrl: './custom-agent-dialog.html',
  styleUrl: './custom-agent-dialog.scss',
})
export class CustomAgentDialogComponent {
  readonly dialogRef = inject(MatDialogRef<CustomAgentDialogComponent>);
  private readonly state = inject(AppStateService);
  readonly detail = inject<CustomAgentDialogData>(MAT_DIALOG_DATA).detail;

  readonly id = signal(this.detail?.id ?? '');
  readonly displayName = signal(this.detail?.display_name ?? '');
  readonly command = signal(this.detail?.command ?? '');
  readonly argsText = signal((this.detail?.args ?? []).join('\n'));
  readonly envText = signal(
    Object.entries(this.detail?.env ?? {})
      .map(([key, value]) => `${key}=${value}`)
      .join('\n'),
  );
  readonly idleTimeout = signal(String(this.detail?.idle_timeout ?? 900));
  readonly usageProvider = signal(this.detail?.usage_provider ?? '');
  readonly description = signal(this.detail?.description ?? '');
  readonly metadataText = signal(
    this.detail && this.detail.metadata != null ? JSON.stringify(this.detail.metadata, null, 2) : '',
  );

  readonly report = signal<ValidationReport | null>(null);
  readonly error = signal('');
  readonly saving = signal(false);
  readonly validating = signal(false);

  get editing(): boolean {
    return this.detail !== null;
  }

  issueFor(field: string): string {
    return this.report()?.issues.find((issue) => issue.field === field)?.message ?? '';
  }

  async validate(): Promise<void> {
    const input = this.buildInput();
    if (!input) return;
    this.validating.set(true);
    this.error.set('');
    try {
      this.report.set(await this.state.validateCustomAgent(input));
    } catch (err: unknown) {
      this.error.set(this.message(err, 'Failed to validate the definition'));
    } finally {
      this.validating.set(false);
    }
  }

  async save(): Promise<void> {
    const input = this.buildInput();
    if (!input) return;
    this.saving.set(true);
    this.error.set('');
    try {
      const report = await this.state.validateCustomAgent(input);
      this.report.set(report);
      if (!report.valid) return;
      if (this.editing) {
        await this.state.editCustomAgent(this.detail!.id, input);
      } else {
        await this.state.createCustomAgent(input);
      }
      this.dialogRef.close(true);
    } catch (err: unknown) {
      this.error.set(this.message(err, 'Failed to save the definition'));
    } finally {
      this.saving.set(false);
    }
  }

  private buildInput(): CustomAgentInput | null {
    const metadata = this.parseMetadata();
    if (metadata === undefined) return null;
    const timeout = Number(this.idleTimeout());
    return {
      id: this.id().trim(),
      display_name: this.displayName().trim() || null,
      command: this.command(),
      args: this.argsText()
        .split('\n')
        .map((line) => line.trim())
        .filter((line) => line.length > 0),
      env: this.parseEnv(this.envText()),
      idle_timeout: Number.isFinite(timeout) && timeout > 0 ? timeout : null,
      usage_provider: this.usageProvider().trim() || null,
      metadata,
      description: this.description().trim() || null,
    };
  }

  private parseEnv(text: string): Record<string, string> {
    const env: Record<string, string> = {};
    for (const line of text.split('\n')) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      const index = trimmed.indexOf('=');
      if (index <= 0) continue;
      env[trimmed.slice(0, index).trim()] = trimmed.slice(index + 1);
    }
    return env;
  }

  private parseMetadata(): unknown | undefined {
    const text = this.metadataText().trim();
    if (!text) return null;
    try {
      return JSON.parse(text);
    } catch {
      this.error.set('Metadata must be valid JSON.');
      return undefined;
    }
  }

  private message(error: unknown, fallback: string): string {
    return error instanceof Error && error.message ? error.message : fallback;
  }
}
