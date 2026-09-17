import { ComponentFixture, TestBed } from '@angular/core/testing';
import { describe, expect, it, beforeEach, vi } from 'vitest';
import { PermissionCardComponent } from './permission-card';
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
      kind: 'switch_mode',
      description: '### Proposed Plan\n\n1. Step one\n2. Step two',
      options: [
        { optionId: 'reject', name: 'Reject', kind: 'reject_once' },
        { optionId: 'approve', name: 'Approve Plan', kind: 'allow_once' },
      ],
      responded: false,
    };
    fixture.componentRef.setInput('permission', planPerm);
    fixture.detectChanges();

    expect(component.isPlanApproval()).toBe(true);
    expect(component.icon()).toBe('assignment_turned_in');

    const heading = fixture.nativeElement.querySelector('.permission-title');
    expect(heading.textContent).toBe('Approve Plan');

    const markdownHost = fixture.nativeElement.querySelector('hub-markdown');
    expect(markdownHost).not.toBeNull();
    expect(markdownHost.textContent).toContain('Proposed Plan');

    const buttons = fixture.nativeElement.querySelectorAll('button');
    expect(buttons[0].textContent?.trim()).toBe('Reject');
    expect(buttons[1].textContent?.trim()).toBe('Approve Plan');
    expect(buttons[0].getAttribute('aria-label')).toBe('Reject');
    expect(buttons[1].getAttribute('aria-label')).toBe('Approve Plan');

    buttons[1].click();
    await fixture.whenStable();
    expect(responded).toEqual([{ chatId: 'chat-1', requestId: 'perm-1', optionId: 'approve' }]);
    expect(fixture.nativeElement.querySelectorAll('button')).toHaveLength(0);
    expect(fixture.nativeElement.querySelector('.decision')?.textContent).toContain('Approve Plan');
  });

  it('renders permission choices exactly as provided by the agent without scope wording', async () => {
    const genericPerm: TurnEntryPermission = {
      id: 2,
      type: 'permission_request',
      requestId: 'perm-2',
      method: 'bash',
      description: 'cargo build',
      options: [
        { optionId: 'yes', name: 'Yes', kind: 'allow_once' },
        { optionId: 'no', name: 'No', kind: 'reject_once' },
        { optionId: 'allow-once', name: 'Allow once', kind: 'allow_once' },
        {
          optionId: 'allow-always',
          name: "Yes, and don't ask again for cargo clippy * commands",
          kind: 'allow_always',
        },
      ],
      responded: false,
    };

    fixture.componentRef.setInput('permission', genericPerm);
    fixture.detectChanges();

    expect(component.isPlanApproval()).toBe(false);
    expect(component.icon()).toBe('shield_person');

    const heading = fixture.nativeElement.querySelector('.permission-title');
    expect(heading.textContent).toBe('Permission request');

    const buttons = fixture.nativeElement.querySelectorAll('button');
    expect(buttons).toHaveLength(4);

    expect(buttons[0].textContent?.trim()).toBe('Yes');
    expect(buttons[0].getAttribute('aria-label')).toBe('Yes');

    expect(buttons[1].textContent?.trim()).toBe('No');
    expect(buttons[1].getAttribute('aria-label')).toBe('No');

    expect(buttons[2].textContent?.trim()).toBe('Allow once');
    expect(buttons[2].getAttribute('aria-label')).toBe('Allow once');

    expect(buttons[3].textContent?.trim()).toBe("Yes, and don't ask again for cargo clippy * commands");
    expect(buttons[3].getAttribute('aria-label')).toBe("Yes, and don't ask again for cargo clippy * commands");

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

    expect(fixture.nativeElement.querySelector('.review-facts')?.textContent).toContain('Medium');
    expect(fixture.nativeElement.querySelector('.review-facts')?.textContent).toContain('User approval required');
    expect(fixture.nativeElement.querySelector('.review-action')).not.toBeNull();
    expect(fixture.nativeElement.querySelector('.raw-review')).not.toBeNull();
    expect(fixture.nativeElement.querySelector('.raw-review pre')).toBeNull();
    expect(fixture.nativeElement.querySelectorAll('button')).toHaveLength(1);
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
    expect(fixture.nativeElement.querySelector('pre')?.textContent).toContain('Apply this ordinary ACP edit request exactly as supplied.');
  });

  it('collapses resolved structured reviews into a concise historical record', () => {
    fixture.componentRef.setInput('permission', {
      id: 7,
      type: 'permission_request',
      requestId: 'review-2',
      method: 'execute_command',
      description: 'Status: Approved\nAction: cargo test\nRisk: Low\nAuthorization: Allowed once\nRationale: Run focused tests.',
      options: [],
      responded: true,
      decision: 'Allowed once',
    });
    fixture.detectChanges();

    expect(fixture.nativeElement.querySelector('.decision')?.textContent).toContain('Allowed once');
    expect(fixture.nativeElement.querySelector('.review-details')).not.toBeNull();
    expect(fixture.nativeElement.querySelector('.review-details pre')).toBeNull();
    expect(fixture.nativeElement.querySelectorAll('button')).toHaveLength(0);
  });

  it('locks every choice until the response succeeds', async () => {
    let resolveResponse!: () => void;
    respondPermission.mockImplementationOnce(() => new Promise<void>((resolve) => {
      resolveResponse = resolve;
    }));
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
    expect(fixture.nativeElement.querySelector('[role="alert"]').textContent).toContain('Permission service unavailable');
    expect(fixture.nativeElement.querySelector('button').disabled).toBe(false);

    fixture.nativeElement.querySelector('button').click();
    await fixture.whenStable();
    fixture.detectChanges();
    expect(respondPermission).toHaveBeenCalledTimes(2);
    expect(fixture.nativeElement.querySelectorAll('button')).toHaveLength(0);
  });
});
