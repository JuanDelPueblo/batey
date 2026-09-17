import { Component, computed, inject, input, signal } from '@angular/core';
import { NgTemplateOutlet } from '@angular/common';
import { MatButtonModule } from '@angular/material/button';
import { MatCardModule } from '@angular/material/card';
import { MatExpansionModule } from '@angular/material/expansion';
import { MatIconModule } from '@angular/material/icon';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import type { TurnEntryPermission } from '../../core/api/types';
import { MarkdownComponent } from '../../shared/markdown/markdown.component';
import { AppStateService } from '../../state/app-state.service';

@Component({
  selector: 'hub-permission-card',
  imports: [
    NgTemplateOutlet,
    MatButtonModule,
    MatCardModule,
    MatExpansionModule,
    MatIconModule,
    MatProgressSpinnerModule,
    MarkdownComponent,
  ],
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
    () =>
      this.permission().title ||
      this.review()?.title ||
      (this.isPlanApproval() ? 'Approve Plan' : 'Permission request'),
  );

  readonly icon = computed(() => (this.isPlanApproval() ? 'assignment_turned_in' : 'shield_person'));
  readonly isResolved = computed(
    () => this.permission().responded || this.resolvedRequestId() === this.permission().requestId,
  );
  readonly displayDecision = computed(
    () => this.permission().decision || this.resolvedDecision() || 'Handled',
  );
  readonly review = computed(() => parseStructuredReview(this.permission().description));

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
      this.responseError.set(
        error instanceof Error ? error.message : 'Could not send the permission response. Try again.',
      );
    } finally {
      this.responding.set(false);
      this.selectedOptionId.set(null);
    }
  }
}

export type ReviewField = 'status' | 'action' | 'risk' | 'authorization' | 'rationale';

export interface StructuredReview {
  readonly title?: string;
  readonly status: string;
  readonly action: string;
  readonly risk: string;
  readonly authorization: string;
  readonly rationale: string;
  readonly unmatchedDetails?: string;
}

const reviewFields: readonly ReviewField[] = ['status', 'action', 'risk', 'authorization', 'rationale'];
const reviewFieldPattern =
  /^(?:#{1,6}\s+|[-*]\s+)?(?:\*\*)?(Status|Action|Risk|Authorization|Rationale)(?:\s*\*\*\s*:|\s*:\s*\*\*|\s*\*\*|\s*:)\s*:*\s*(.*)$/i;
const reviewHeaderPattern =
  /^(?:#{1,6}\s+.*|(?:\*\*)?[A-Za-z0-9 _-]*(?:Review|Request)(?:\s*:\s*\*\*|\s*\*\*\s*:?|\s*:)?)\s*$/i;

/** Recognize only the complete, label-based review format; all other ACP text stays raw. */
export function parseStructuredReview(description: string): StructuredReview | null {
  const values = new Map<ReviewField, string>();
  const unmatchedLines: string[] = [];
  let headerTitle: string | undefined;
  let current: ReviewField | null = null;
  let pendingBlankLines: string[] = [];

  for (const rawLine of description.replace(/\r\n?/g, '\n').split('\n')) {
    const line = rawLine.trimEnd();
    const trimmed = line.trim();

    const match = trimmed.match(reviewFieldPattern);
    if (match) {
      pendingBlankLines = [];
      current = match[1].toLowerCase() as ReviewField;
      values.set(current, match[2].replace(/^:+/, '').trim());
      continue;
    }

    if (current === null) {
      if (!trimmed) continue;
      if (!headerTitle && reviewHeaderPattern.test(trimmed)) {
        headerTitle = trimmed
          .replace(/^#{1,6}\s+/, '')
          .replace(/^[*_:\s]+|[*_:\s]+$/g, '')
          .trim();
      } else {
        unmatchedLines.push(line);
      }
      continue;
    }

    if (!trimmed) {
      pendingBlankLines.push(line);
      continue;
    }

    const hasAllFields = reviewFields.every((f) => values.has(f));
    const isSectionLabel = /^(?:#{1,6}\s+|[-*]\s+)?(?:\*\*)?[A-Za-z0-9 _-]+(?:\s*\*\*\s*:|\s*:\s*\*\*|\s*\*\*|\s*:)\s*:?/i.test(
      trimmed,
    );

    if (hasAllFields && (pendingBlankLines.length > 0 || isSectionLabel)) {
      current = null;
      if (pendingBlankLines.length > 0 && unmatchedLines.length > 0) {
        unmatchedLines.push(...pendingBlankLines);
      }
      pendingBlankLines = [];
      unmatchedLines.push(line);
      continue;
    }

    if (pendingBlankLines.length > 0) {
      values.set(current, (values.get(current) ?? '') + '\n' + pendingBlankLines.join('\n'));
      pendingBlankLines = [];
    }
    const existing = values.get(current) ?? '';
    values.set(current, existing ? existing + '\n' + line : line);
  }

  if (!reviewFields.every((field) => values.has(field))) return null;

  const unmatchedText = unmatchedLines.join('\n').trim();
  return {
    status: values.get('status') ?? '',
    action: values.get('action') ?? '',
    risk: values.get('risk') ?? '',
    authorization: values.get('authorization') ?? '',
    rationale: values.get('rationale') ?? '',
    ...(headerTitle ? { title: headerTitle } : {}),
    ...(unmatchedText ? { unmatchedDetails: unmatchedText } : {}),
  };
}
