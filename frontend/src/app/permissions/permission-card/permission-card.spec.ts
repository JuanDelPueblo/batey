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
    expect(buttons[0].textContent).toContain('Reject');
    expect(buttons[1].textContent).toContain('Approve Plan');
    expect(buttons[0].getAttribute('aria-label')).toBe('Reject, One time');
    expect(buttons[1].getAttribute('aria-label')).toBe('Approve Plan, One time');

    buttons[1].click();
    await fixture.whenStable();
    expect(responded).toEqual([{ chatId: 'chat-1', requestId: 'perm-1', optionId: 'approve' }]);
    expect(fixture.nativeElement.querySelectorAll('button')).toHaveLength(0);
    expect(fixture.nativeElement.querySelector('.decision')?.textContent).toContain('Approve Plan');
  });

  it('renders generic permission choices including persistent scope', async () => {
    const genericPerm: TurnEntryPermission = {
      id: 2,
      type: 'permission_request',
      requestId: 'perm-2',
      method: 'bash',
      description: 'cargo build',
      options: [
        { optionId: 'deny', name: 'Deny', kind: 'reject_once' },
        { optionId: 'allow', name: 'Allow', kind: 'allow_always' },
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
    expect(buttons[0].textContent).toContain('Deny');
    expect(buttons[1].textContent).toContain('Allow');
    expect(buttons[0].querySelector('.option-scope')?.textContent).toBe('One time');
    expect(buttons[1].querySelector('.option-scope')?.textContent).toBe('Persistent');
    expect(buttons[0].querySelector('.option-content')?.textContent.replace(/\s+/g, ' ').trim())
      .toBe('Deny One time');
    expect(buttons[1].querySelector('.option-content')?.textContent.replace(/\s+/g, ' ').trim())
      .toBe('Allow Persistent');

    buttons[0].click();
    await fixture.whenStable();
    expect(responded).toEqual([{ chatId: 'chat-1', requestId: 'perm-2', optionId: 'deny' }]);
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
