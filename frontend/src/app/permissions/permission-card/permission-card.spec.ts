import { ComponentFixture, TestBed } from '@angular/core/testing';
import { describe, expect, it, beforeEach } from 'vitest';
import { PermissionCardComponent } from './permission-card';
import { AppStateService } from '../../state/app-state.service';
import type { TurnEntryPermission } from '../../core/api/types';

describe('PermissionCardComponent', () => {
  let fixture: ComponentFixture<PermissionCardComponent>;
  let component: PermissionCardComponent;
  const responded: Array<{ chatId: string; requestId: string; optionId: string }> = [];

  beforeEach(async () => {
    responded.length = 0;
    await TestBed.configureTestingModule({
      imports: [PermissionCardComponent],
      providers: [
        {
          provide: AppStateService,
          useValue: {
            respondPermission: async (chatId: string, requestId: string, optionId: string) => {
              responded.push({ chatId, requestId, optionId });
            },
          },
        },
      ],
    }).compileComponents();
    fixture = TestBed.createComponent(PermissionCardComponent);
    component = fixture.componentInstance;
    fixture.componentRef.setInput('chatId', 'chat-1');
  });

  it('renders plan approval with markdown and Approve Plan action', async () => {
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
    expect(buttons[0].textContent.trim()).toBe('Reject');
    expect(buttons[1].textContent.trim()).toBe('Approve Plan');

    buttons[1].click();
    await fixture.whenStable();
    expect(responded).toEqual([{ chatId: 'chat-1', requestId: 'perm-1', optionId: 'approve' }]);
  });

  it('renders generic permission with Allow and Deny buttons', async () => {
    const genericPerm: TurnEntryPermission = {
      id: 2,
      type: 'permission_request',
      requestId: 'perm-2',
      method: 'bash',
      description: 'cargo build',
      options: [
        { optionId: 'deny', name: 'Deny', kind: 'reject_once' },
        { optionId: 'allow', name: 'Allow', kind: 'allow_once' },
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
    expect(buttons[0].textContent.trim()).toBe('Deny');
    expect(buttons[1].textContent.trim()).toBe('Allow');

    buttons[0].click();
    await fixture.whenStable();
    expect(responded).toEqual([{ chatId: 'chat-1', requestId: 'perm-2', optionId: 'deny' }]);
  });
});
