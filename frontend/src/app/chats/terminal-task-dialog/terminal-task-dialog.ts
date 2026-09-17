import { Component, inject, OnInit, signal } from '@angular/core';
import { MAT_DIALOG_DATA, MatDialogModule, MatDialogRef } from '@angular/material/dialog';
import { MatButtonModule } from '@angular/material/button';
import { MatIconModule } from '@angular/material/icon';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import { MatTooltipModule } from '@angular/material/tooltip';
import { ApiService } from '../../core/api/api.service';
import type { Chat, TerminalTaskDetails, TerminalTaskSummary } from '../../core/api/types';
import { formatElapsed, formatLocalDateTime } from '../../state/chat-activity';

@Component({
  selector: 'hub-terminal-task-dialog',
  imports: [
    MatButtonModule,
    MatDialogModule,
    MatIconModule,
    MatProgressSpinnerModule,
    MatTooltipModule,
  ],
  templateUrl: './terminal-task-dialog.html',
  styleUrl: './terminal-task-dialog.scss',
})
export class TerminalTaskDialogComponent implements OnInit {
  readonly dialogRef = inject(MatDialogRef<TerminalTaskDialogComponent>);
  readonly chat = inject<Chat>(MAT_DIALOG_DATA);
  private readonly api = inject(ApiService);

  readonly tasks = signal<TerminalTaskSummary[]>([]);
  readonly selectedTaskId = signal<string | null>(null);
  readonly selectedTaskDetails = signal<TerminalTaskDetails | null>(null);
  readonly loading = signal(true);
  readonly loadingDetails = signal(false);
  readonly stopping = signal(false);
  readonly error = signal('');

  ngOnInit(): void {
    void this.loadTasks();
  }

  async loadTasks(): Promise<void> {
    this.loading.set(true);
    this.error.set('');
    try {
      const list = await this.api.fetchChatTasks(this.chat.id);
      this.tasks.set(list);
      if (list.length > 0) {
        const currentId = this.selectedTaskId();
        const preferred = currentId && list.some((t) => t.id === currentId)
          ? currentId
          : (list.find((t) => t.state === 'running') ?? list[0]).id;
        await this.selectTask(preferred);
      } else {
        this.selectedTaskId.set(null);
        this.selectedTaskDetails.set(null);
      }
    } catch (err: unknown) {
      this.error.set(err instanceof Error ? err.message : 'Failed to load terminal tasks');
    } finally {
      this.loading.set(false);
    }
  }

  async selectTask(taskId: string): Promise<void> {
    this.selectedTaskId.set(taskId);
    this.loadingDetails.set(true);
    try {
      const details = await this.api.fetchChatTask(this.chat.id, taskId);
      this.selectedTaskDetails.set(details);
    } catch (err: unknown) {
      this.error.set(err instanceof Error ? err.message : 'Failed to load task details');
    } finally {
      this.loadingDetails.set(false);
    }
  }

  async stopTask(taskId: string): Promise<void> {
    this.stopping.set(true);
    try {
      await this.api.stopChatTask(this.chat.id, taskId);
      await this.loadTasks();
      if (this.selectedTaskId() === taskId) {
        await this.selectTask(taskId);
      }
    } catch (err: unknown) {
      this.error.set(err instanceof Error ? err.message : 'Failed to stop task');
    } finally {
      this.stopping.set(false);
    }
  }

  /** Agent-owned observational tasks report managed:false and cannot be stopped. */
  isTaskStoppable(task: TerminalTaskSummary | TerminalTaskDetails | null | undefined): boolean {
    return !!task && task.state === 'running' && task.managed !== false;
  }

  taskDuration(task: TerminalTaskSummary): string {
    const start = Date.parse(task.started_at);
    if (!Number.isFinite(start)) return '';
    const end = task.completed_at ? Date.parse(task.completed_at) : Date.now();
    return formatElapsed(Math.max(0, end - start));
  }

  formatDateTime(value: string): string {
    return formatLocalDateTime(value);
  }
}
