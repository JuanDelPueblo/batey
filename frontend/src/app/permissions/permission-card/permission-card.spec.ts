import { ComponentFixture, TestBed } from '@angular/core/testing';
import { describe, expect, it, beforeEach, vi } from 'vitest';
import { parseStructuredReview, PermissionCardComponent } from './permission-card';
import { AppStateService } from '../../state/app-state.service';
import type { TurnEntryPermission } from '../../core/api/types';

describe('PermissionCardComponent', () => {
  let fixture: ComponentFixture<PermissionCardComponent>;
  let component: PermissionCardComponent;
  const responded: Array<{ chatId: string; requestId: string; optionId: string }> = [];
  let respondPermission: ReturnType<typeof vi.fn>;

  beforeEach(async () => {
    responded.length = 0;
    respondPermission = vi.fn(async (chatId: string, requestId: string, optionId: string) => {
      responded.push({ chatId, requestId, optionId });
    });
    await TestBed.configureTestingModule({
      imports: [PermissionCardComponent],
      providers: [
        {
          provide: AppStateService,
          useValue: {
            respondPermission,
          },
        },
      ],
    }).compileComponents();
    fixture = TestBed.createComponent(PermissionCardComponent);
    component = fixture.componentInstance;
    fixture.componentRef.setInput('chatId', 'chat-1');
  });

  it('renders plan approval with markdown and every agent-provided action', async () => {
    const planPerm: TurnEntryPermission = {
      id: 1,
      type: 'permission_request',
      requestId: 'perm-1',
      method: 'session/request_permission',
      title: 'Approve Plan',
      description: '### Proposed Steps\n\n1. Run migrations\n2. Deploy services\n3. Verify health',
      options: [
        { optionId: 'approve', name: 'Approve Plan', kind: 'allow_once' },
        { optionId: 'reject', name: 'Reject Plan', kind: 'deny' },
      ],
      responded: false,
    };
    fixture.componentRef.setInput('permission', planPerm);
    fixture.detectChanges();

    expect(fixture.nativeElement.querySelector('.permission-title')?.textContent?.trim()).toBe('Approve Plan');
    expect(fixture.nativeElement.querySelector('.permission-method')).toBeNull();
    expect(fixture.nativeElement.querySelector('mat-icon')?.textContent?.trim()).toBe('assignment_turned_in');

    const steps = fixture.nativeElement.querySelectorAll('.plan-content li');
    expect(steps).toHaveLength(3);
    expect(steps[0].textContent).toContain('Run migrations');
    expect(steps[1].textContent).toContain('Deploy services');
    expect(steps[2].textContent).toContain('Verify health');

    const buttons = fixture.nativeElement.querySelectorAll('button');
    expect(buttons).toHaveLength(2);
    expect(buttons[0].textContent?.trim()).toBe('Approve Plan');
    expect(buttons[1].textContent?.trim()).toBe('Reject Plan');

    buttons[0].click();
    await fixture.whenStable();
    expect(responded).toEqual([{ chatId: 'chat-1', requestId: 'perm-1', optionId: 'approve' }]);
  });

  it('renders standard permissions with shield icon, method, raw description, and raw option names', async () => {
    fixture.componentRef.setInput('permission', {
      id: 2,
      type: 'permission_request',
      requestId: 'perm-2',
      method: 'execute_command',
      description: 'Run cargo clippy --fix',
      options: [
        { optionId: 'allow_once', name: 'Yes', kind: 'allow_once' },
        { optionId: 'deny', name: 'No', kind: 'deny' },
        { optionId: 'allow-once', name: 'Allow once', kind: 'allow_once' },
        {
          optionId: 'allow-always',
          name: "Yes, and don't ask again for cargo clippy * commands",
          kind: 'allow_always',
        },
      ],
      responded: false,
    });
    fixture.detectChanges();

    expect(fixture.nativeElement.querySelector('.permission-title')?.textContent?.trim()).toBe(
      'Permission request',
    );
    expect(fixture.nativeElement.querySelector('.permission-method')?.textContent?.trim()).toBe(
      'execute_command',
    );
    expect(fixture.nativeElement.querySelector('mat-icon')?.textContent?.trim()).toBe('shield_person');
    expect(fixture.nativeElement.querySelector('pre')?.textContent).toContain('Run cargo clippy --fix');

    const buttons = fixture.nativeElement.querySelectorAll('button');
    expect(buttons).toHaveLength(4);

    expect(buttons[0].textContent?.trim()).toBe('Yes');
    expect(buttons[0].getAttribute('aria-label')).toBe('Yes');

    expect(buttons[1].textContent?.trim()).toBe('No');
    expect(buttons[1].getAttribute('aria-label')).toBe('No');

    expect(buttons[2].textContent?.trim()).toBe('Allow once');
    expect(buttons[2].getAttribute('aria-label')).toBe('Allow once');

    expect(buttons[3].textContent?.trim()).toBe("Yes, and don't ask again for cargo clippy * commands");
    expect(buttons[3].getAttribute('aria-label')).toBe(
      "Yes, and don't ask again for cargo clippy * commands",
    );

    for (const button of buttons) {
      expect(button.textContent).not.toContain('One time');
      expect(button.textContent).not.toContain('Persistent');
      expect(button.getAttribute('aria-label')).not.toContain('One time');
      expect(button.getAttribute('aria-label')).not.toContain('Persistent');
    }

    buttons[3].click();
    await fixture.whenStable();
    expect(responded).toEqual([{ chatId: 'chat-1', requestId: 'perm-2', optionId: 'allow-always' }]);
  });

  it('renders a complete review as structured details without repeating its raw content', () => {
    fixture.componentRef.setInput('permission', {
      id: 5,
      type: 'permission_request',
      requestId: 'review-1',
      method: 'execute_command',
      title: 'Review request',
      description: `Guardian Review\nStatus: Pending\nAction: nix run .#verify -- --a-command-that-is-deliberately-very-long\nRisk: Medium\nAuthorization: User approval required\nRationale: Verify the requested change.`,
      options: [{ optionId: 'allow', name: 'Allow', kind: 'allow_once' }],
      responded: false,
    });
    fixture.detectChanges();

    const rootText = fixture.nativeElement.textContent ?? '';
    expect(fixture.nativeElement.querySelector('.review-facts')?.textContent).toContain('Medium');
    expect(fixture.nativeElement.querySelector('.review-facts')?.textContent).toContain(
      'User approval required',
    );
    expect(fixture.nativeElement.querySelector('.review-action')).not.toBeNull();
    expect(fixture.nativeElement.querySelector('.review-action')?.textContent).toContain(
      'nix run .#verify -- --a-command-that-is-deliberately-very-long',
    );

    // Each review field is rendered exactly once across the whole card
    expect((rootText.match(/Status/g) ?? []).length).toBe(1);
    expect((rootText.match(/Risk/g) ?? []).length).toBe(1);
    expect((rootText.match(/Authorization/g) ?? []).length).toBe(1);
    expect((rootText.match(/Rationale/g) ?? []).length).toBe(1);

    // No raw review fallback or extra details panel rendered when all content is consumed
    expect(fixture.nativeElement.querySelector('.raw-review')).toBeNull();
    expect(fixture.nativeElement.querySelector('.review-additional')).toBeNull();

    // The raw Status: ... Action: ... block is not rendered as raw pre text
    expect(fixture.nativeElement.querySelector('pre')).toBeNull();
    expect(fixture.nativeElement.querySelectorAll('button')).toHaveLength(1);
  });

  it('retains unmatched extra text as optional additional details without repeating parsed fields', () => {
    fixture.componentRef.setInput('permission', {
      id: 55,
      type: 'permission_request',
      requestId: 'review-extra',
      method: 'execute_command',
      title: 'Review request',
      description: `Guardian Review\nStatus: Pending\nAction: nix run .#verify\nRisk: Medium\nAuthorization: User approval required\nRationale: Verify the requested change.\n\nAdditional notes:\nRun within nix develop shell.`,
      options: [{ optionId: 'allow', name: 'Allow', kind: 'allow_once' }],
      responded: false,
    });
    fixture.detectChanges();

    expect(fixture.nativeElement.querySelector('.review-facts')?.textContent).toContain('Medium');
    expect(fixture.nativeElement.querySelector('.review-action')).not.toBeNull();

    const additionalPanel = fixture.nativeElement.querySelector('.review-additional');
    expect(additionalPanel).not.toBeNull();
    expect(additionalPanel.textContent).toContain('Additional details');

    // When expanded, Additional details contains ONLY the unmatched text, not repeated parsed fields
    additionalPanel.querySelector('mat-expansion-panel-header')?.click();
    fixture.detectChanges();

    const additionalPre = additionalPanel.querySelector('pre');
    expect(additionalPre?.textContent).toContain('Run within nix develop shell.');
    expect(additionalPre?.textContent).not.toContain('Status: Pending');
    expect(additionalPre?.textContent).not.toContain('Rationale: Verify the requested change.');
  });

  it('keeps non-review permission text as a faithful raw fallback', () => {
    fixture.componentRef.setInput('permission', {
      id: 6,
      type: 'permission_request',
      requestId: 'generic-1',
      method: 'edit',
      description: 'Apply this ordinary ACP edit request exactly as supplied.',
      options: [],
      responded: false,
    });
    fixture.detectChanges();

    expect(fixture.nativeElement.querySelector('.review')).toBeNull();
    expect(fixture.nativeElement.querySelector('pre')?.textContent).toContain(
      'Apply this ordinary ACP edit request exactly as supplied.',
    );
  });

  it('collapses resolved structured reviews into a concise historical record without repeating content', () => {
    fixture.componentRef.setInput('permission', {
      id: 7,
      type: 'permission_request',
      requestId: 'review-2',
      method: 'execute_command',
      description:
        'Status: Approved\nAction: cargo test\nRisk: Low\nAuthorization: Allowed once\nRationale: Run focused tests.',
      options: [],
      responded: true,
      decision: 'Allowed once',
    });
    fixture.detectChanges();

    expect(fixture.nativeElement.querySelector('.decision')?.textContent).toContain('Allowed once');
    const detailsPanel = fixture.nativeElement.querySelector('.review-details');
    expect(detailsPanel).not.toBeNull();
    expect(fixture.nativeElement.querySelector('.review-details pre')).toBeNull();
    expect(fixture.nativeElement.querySelectorAll('button')).toHaveLength(0);

    // Expanding review details reveals the structured details without any duplicate raw review
    detailsPanel.querySelector('mat-expansion-panel-header')?.click();
    fixture.detectChanges();

    expect(detailsPanel.querySelector('.review-facts')?.textContent).toContain('Approved');
    expect(detailsPanel.querySelector('.review-facts')?.textContent).toContain('Run focused tests.');
    expect(detailsPanel.querySelector('.raw-review')).toBeNull();
    expect(detailsPanel.querySelector('.review-additional')).toBeNull();
  });

  it('locks every choice until the response succeeds', async () => {
    let resolveResponse!: () => void;
    respondPermission.mockImplementationOnce(
      () =>
        new Promise<void>((resolve) => {
          resolveResponse = resolve;
        }),
    );
    fixture.componentRef.setInput('permission', {
      id: 3,
      type: 'permission_request',
      requestId: 'perm-3',
      method: 'edit',
      description: 'Edit',
      options: [
        { optionId: 'one', name: 'One', kind: 'allow_once' },
        { optionId: 'two', name: 'Two', kind: 'allow_always' },
      ],
    });
    fixture.detectChanges();

    const buttons = fixture.nativeElement.querySelectorAll('button');
    buttons[0].click();
    fixture.detectChanges();
    expect([...buttons].every((button: HTMLButtonElement) => button.disabled)).toBe(true);
    buttons[1].click();
    expect(respondPermission).toHaveBeenCalledTimes(1);

    resolveResponse();
    await fixture.whenStable();
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelectorAll('button')).toHaveLength(0);
  });

  it('restores choices and shows a retryable error after failure', async () => {
    respondPermission
      .mockRejectedValueOnce(new Error('Permission service unavailable'))
      .mockResolvedValueOnce(undefined);
    fixture.componentRef.setInput('permission', {
      id: 4,
      type: 'permission_request',
      requestId: 'perm-4',
      method: 'edit',
      description: 'Edit',
      options: [{ optionId: 'retry', name: 'Try it', kind: 'allow_once' }],
    });
    fixture.detectChanges();

    fixture.nativeElement.querySelector('button').click();
    await fixture.whenStable();
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelector('[role="alert"]').textContent).toContain(
      'Permission service unavailable',
    );
    expect(fixture.nativeElement.querySelector('button').disabled).toBe(false);

    fixture.nativeElement.querySelector('button').click();
    await fixture.whenStable();
    fixture.detectChanges();
    expect(respondPermission).toHaveBeenCalledTimes(2);
    expect(fixture.nativeElement.querySelectorAll('button')).toHaveLength(0);
  });
});

describe('parseStructuredReview', () => {
  it('parses the exact five-field Guardian review without unmatched details', () => {
    const raw = `Guardian Review\nStatus: Pending\nAction: nix run .#verify -- --a-command-that-is-deliberately-very-long\nRisk: Medium\nAuthorization: User approval required\nRationale: Verify the requested change.`;
    const result = parseStructuredReview(raw);

    expect(result).toEqual({
      title: 'Guardian Review',
      status: 'Pending',
      action: 'nix run .#verify -- --a-command-that-is-deliberately-very-long',
      risk: 'Medium',
      authorization: 'User approval required',
      rationale: 'Verify the requested change.',
    });
    expect(result?.unmatchedDetails).toBeUndefined();
  });

  it('parses 5 fields alone without header or unmatched details', () => {
    const raw = `Status: Pending\nAction: cargo test\nRisk: Low\nAuthorization: Allowed once\nRationale: Run focused tests.`;
    const result = parseStructuredReview(raw);

    expect(result).toEqual({
      status: 'Pending',
      action: 'cargo test',
      risk: 'Low',
      authorization: 'Allowed once',
      rationale: 'Run focused tests.',
    });
    expect(result?.unmatchedDetails).toBeUndefined();
    expect(result?.title).toBeUndefined();
  });

  it('preserves unmatched pre-text at the top', () => {
    const raw = `Important note: do not run in production!\nStatus: Pending\nAction: cargo test\nRisk: Low\nAuthorization: Allowed once\nRationale: Run focused tests.`;
    const result = parseStructuredReview(raw);

    expect(result?.status).toBe('Pending');
    expect(result?.unmatchedDetails).toBe('Important note: do not run in production!');
  });

  it('preserves unmatched trailing notes', () => {
    const raw = `Status: Pending\nAction: cargo test\nRisk: Low\nAuthorization: Allowed once\nRationale: Run focused tests.\n\nAdditional notes:\nEnsure database is running.`;
    const result = parseStructuredReview(raw);

    expect(result?.status).toBe('Pending');
    expect(result?.unmatchedDetails).toBe('Additional notes:\nEnsure database is running.');
  });

  it('preserves multi-line action without treating it as unmatched text', () => {
    const raw = `Status: Pending\nAction: git checkout main\ngit pull\nRisk: Low\nAuthorization: Allowed once\nRationale: Update repository.`;
    const result = parseStructuredReview(raw);

    expect(result?.action).toBe('git checkout main\ngit pull');
    expect(result?.unmatchedDetails).toBeUndefined();
  });

  it('returns null for unstructured permission descriptions', () => {
    expect(parseStructuredReview('Apply this ordinary ACP edit request exactly as supplied.')).toBeNull();
    expect(parseStructuredReview('Status: Pending\nAction: cargo test')).toBeNull();
  });
});
