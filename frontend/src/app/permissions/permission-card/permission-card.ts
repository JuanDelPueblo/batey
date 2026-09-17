import { Component, computed, inject, input, signal } from '@angular/core';
import { MatButtonModule } from '@angular/material/button';
import { MatCardModule } from '@angular/material/card';
import { MatIconModule } from '@angular/material/icon';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import type { TurnEntryPermission } from '../../core/api/types';
import { MarkdownComponent } from '../../shared/markdown/markdown.component';
import { AppStateService } from '../../state/app-state.service';

@Component({
  selector: 'hub-permission-card',
  imports: [MatButtonModule, MatCardModule, MatIconModule, MatProgressSpinnerModule, MarkdownComponent],
  templateUrl: './permission-card.html',
  styleUrl: './permission-card.scss',
})
export class PermissionCardComponent {
  readonly permission = input.required<TurnEntryPermission>();
  readonly chatId = input('');
  readonly responding = signal(false);
  readonly responseError = signal<string | null>(null);
  readonly selectedOptionId = signal<string | null>(null);
  private readonly resolvedRequestId = signal<string | null>(null);
  private readonly resolvedDecision = signal<string | null>(null);
  private readonly state = inject(AppStateService);

  readonly isPlanApproval = computed(
    () => this.permission().kind === 'switch_mode' || this.permission().title === 'Approve Plan',
  );

  readonly displayTitle = computed(
    () => this.permission().title || (this.isPlanApproval() ? 'Approve Plan' : 'Permission request'),
  );

  readonly icon = computed(() => (this.isPlanApproval() ? 'assignment_turned_in' : 'shield_person'));
  readonly isResolved = computed(
    () => this.permission().responded || this.resolvedRequestId() === this.permission().requestId,
  );
  readonly displayDecision = computed(
    () => this.permission().decision || this.resolvedDecision() || 'Handled',
  );

  private optionName(optionId: string): string {
    return this.permission().options?.find((option) => option.optionId === optionId)?.name ?? optionId;
  }

  async respond(optionId: string): Promise<void> {
    if (this.responding() || this.isResolved() || !this.chatId() || !this.permission().requestId) return;
    this.responding.set(true);
    this.selectedOptionId.set(optionId);
    this.responseError.set(null);
    try {
      await this.state.respondPermission(this.chatId(), this.permission().requestId, optionId);
      this.resolvedRequestId.set(this.permission().requestId);
      this.resolvedDecision.set(this.optionName(optionId));
    } catch (error) {
      this.responseError.set(error instanceof Error ? error.message : 'Could not send the permission response. Try again.');
    } finally {
      this.responding.set(false);
      this.selectedOptionId.set(null);
    }
  }
}
