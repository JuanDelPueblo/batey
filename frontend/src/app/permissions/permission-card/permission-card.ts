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
  private readonly state = inject(AppStateService);

  readonly isPlanApproval = computed(
    () => this.permission().kind === 'switch_mode' || this.permission().title === 'Approve Plan',
  );

  readonly displayTitle = computed(
    () => this.permission().title || (this.isPlanApproval() ? 'Approve Plan' : 'Permission request'),
  );

  readonly icon = computed(() => (this.isPlanApproval() ? 'assignment_turned_in' : 'shield_person'));


  async respond(optionId: string): Promise<void> {
    if (!this.chatId() || !this.permission().requestId) return;
    this.responding.set(true);
    try {
      await this.state.respondPermission(this.chatId(), this.permission().requestId, optionId);
    } catch (error) {
      console.error('Failed to respond to permission request', error);
    } finally {
      this.responding.set(false);
    }
  }
}
