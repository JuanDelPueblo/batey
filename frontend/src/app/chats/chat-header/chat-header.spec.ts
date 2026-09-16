import { ComponentFixture, TestBed } from '@angular/core/testing';
import { signal } from '@angular/core';
import { describe, expect, it, beforeEach, afterEach, vi } from 'vitest';
import { ChatHeaderComponent } from './chat-header';
import { AppStateService } from '../../state/app-state.service';
import type { Chat } from '../../core/api/types';

describe('ChatHeaderComponent', () => {
  let fixture: ComponentFixture<ChatHeaderComponent>;

  const mockChat: Chat = {
    id: 'chat-1',
    project_id: 'proj-1',
    agent: 'claude',
    title: 'Review the WebSocket replay path',
    acp_session_id: 'acp-1',
    created_at: '2026-09-13T12:00:00Z',
    updated_at: '2026-09-13T12:00:00Z',
    archived: false,
    permission_policy: 'ask',
    config_values: {},
    process_state: 'RUNNING',
    turn_state: 'IDLE',
  };

  const activitySignal = signal('idle');

  beforeEach(async () => {
    activitySignal.set('idle');
    await TestBed.configureTestingModule({
      imports: [ChatHeaderComponent],
      providers: [
        {
          provide: AppStateService,
          useValue: {
            isMobileDrawerOpen: () => false,
            setMobileDrawerOpen: () => undefined,
            stopChatProcess: async () => undefined,
            retryConnection: async () => undefined,
            archiveChat: async () => undefined,
          chatActivity: () => activitySignal(),
          chatTurnStartedAt: () => null,
          },
        },
      ],
    }).compileComponents();

    fixture = TestBed.createComponent(ChatHeaderComponent);
    fixture.componentRef.setInput('chat', mockChat);
    fixture.detectChanges();
  });

  it('renders the agent badge without icon', () => {
    const badge = fixture.nativeElement.querySelector('.badge.agent');
    expect(badge).toBeTruthy();
    expect(badge.textContent.trim()).toBe('Claude');
    expect(badge.querySelector('mat-icon')).toBeNull();
  });

  it('renders the title button with title text', () => {
    const titleBtn = fixture.nativeElement.querySelector('.title-button');
    expect(titleBtn).toBeTruthy();
    expect(titleBtn.textContent).toContain('Review the WebSocket replay path');
  });

  it('renders persisted created and last-updated local date/time values', () => {
    const timestamps = fixture.nativeElement.querySelector('.chat-timestamps') as HTMLElement;
    expect(timestamps.textContent).toContain('Created');
    expect(timestamps.textContent).toContain('Last updated');
    expect(timestamps.querySelectorAll('time')).toHaveLength(2);
    expect(timestamps.querySelector('time')?.getAttribute('datetime')).toBe(mockChat.created_at);
    expect(timestamps.querySelectorAll('time')[1].getAttribute('datetime')).toBe(mockChat.updated_at);
  });

  it('does not render process controls or status labels', () => {
    const text = fixture.nativeElement.textContent;
    expect(text).not.toContain('Running');
    expect(text).not.toContain('Stopped');
    expect(text).not.toContain('Dead');
    expect(fixture.nativeElement.querySelector('button[aria-label="Stop process"]')).toBeNull();
    expect(fixture.nativeElement.querySelector('button[aria-label="Reconnect process"]')).toBeNull();
    expect(fixture.nativeElement.querySelector('button[aria-label="Reconnect ACP"]')).toBeNull();
  });

  it('shows the shared activity badge instead of a hard-coded Thinking label', () => {
    const badge = fixture.nativeElement.querySelector('hub-chat-status-badge .chat-status');
    expect(badge).toBeTruthy();
    expect(badge.textContent.trim()).toBe('Idle');
    expect(fixture.nativeElement.textContent).not.toContain('Thinking…');
  });

  it('reflects the store activity for working, waiting, and error', () => {
    for (const [activity, label] of [['working', 'Working…'], ['waiting', 'Waiting for you'], ['error', 'Error']] as const) {
      activitySignal.set(activity);
      fixture.detectChanges();
      const badge = fixture.nativeElement.querySelector('hub-chat-status-badge .chat-status');
      expect(badge.textContent.trim()).toBe(label);
    }
    const text = fixture.nativeElement.textContent as string;
    for (const process of ['STARTING', 'RUNNING', 'STOPPED', 'DEAD', 'Starting', 'Running', 'Stopped', 'Dead']) {
      expect(text).not.toContain(process);
    }
  });

  it('keeps rename available while the active turn is working', () => {
    fixture.componentRef.setInput('chat', { ...mockChat, turn_state: 'PROMPTING' });
    fixture.detectChanges();

    const titleButton = fixture.nativeElement.querySelector('.title-button') as HTMLButtonElement;
    expect(titleButton.disabled).toBe(false);
  });

  it('renders a truncated branch badge with the full mode tooltip', () => {
    const branch = 'batey/chat/chat-1-with-a-deliberately-long-generated-branch-name';
    fixture.componentRef.setInput('chat', { ...mockChat, workspace: {
      mode: 'managed_worktree', branch, base_commit: 'abcdef1234567890',
    } });
    fixture.detectChanges();

    const badge = fixture.nativeElement.querySelector('.workspace-badge') as HTMLElement;
    expect(badge).toBeTruthy();
    expect(badge.textContent).toContain(branch);
    expect(badge.querySelector('mat-icon')?.textContent.trim()).toBe('account_tree');
    expect(badge.getAttribute('aria-label')).toBe(`Copy branch name: ${branch}`);
    expect(fixture.componentInstance.workspaceTooltip({ mode: 'managed_worktree', branch, base_commit: null }))
      .toBe(`${branch} — Isolated worktree`);
    expect(fixture.componentInstance.workspaceTooltip({ mode: 'project_checkout', branch, base_commit: null }))
      .toBe(`${branch} — Project checkout`);
  });

  it('does not render a workspace badge when workspace or branch is absent', () => {
    fixture.componentRef.setInput('chat', { ...mockChat, workspace: null });
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelector('.workspace-badge')).toBeNull();
    fixture.componentRef.setInput('chat', { ...mockChat, workspace: {
      mode: 'project_checkout', branch: null, base_commit: null,
    } });
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelector('.workspace-badge')).toBeNull();
  });

  it('renders terminal task badge when active tasks exist and hides when zero', () => {
    fixture.componentRef.setInput('chat', { ...mockChat, active_tasks: 2 });
    fixture.detectChanges();
    const badge = fixture.nativeElement.querySelector('.tasks-badge');
    expect(badge).toBeTruthy();
    expect(badge.textContent).toContain('2 active tasks');

    fixture.componentRef.setInput('chat', { ...mockChat, active_tasks: 0 });
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelector('.tasks-badge')).toBeNull();
  });

  it('renders terminal tasks as a header action instead of a menu item', () => {
    const tasksButton = fixture.nativeElement.querySelector('.tasks-button') as HTMLButtonElement;
    const configButton = fixture.nativeElement.querySelector('.config-button') as HTMLButtonElement;
    expect(tasksButton).toBeTruthy();
    expect(tasksButton.getAttribute('aria-label')).toBe('Terminal tasks');
    expect(tasksButton.compareDocumentPosition(configButton) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(fixture.nativeElement.querySelector('mat-menu')?.textContent ?? '').not.toContain('Terminal tasks');
  });

  it('shows context and cost as separate compact indicators', () => {
    fixture.componentRef.setInput('usage', { used: 1200, size: 200000, cost_amount: 0.012, cost_currency: 'USD' });
    fixture.detectChanges();

    const context = fixture.nativeElement.querySelector('.context-badge') as HTMLElement;
    const cost = fixture.nativeElement.querySelector('.cost-badge') as HTMLElement;
    expect(context.textContent.trim()).toBe('Context 1%');
    expect(cost.textContent.trim()).toBe('$0.012');
    expect(fixture.nativeElement.textContent).not.toContain('1,200/200,000 (1%)');

    const tooltip = fixture.componentInstance.contextTooltip() ?? '';
    expect(tooltip).toContain('1,200');
    expect(tooltip).toContain('200,000');
    expect(tooltip).toContain('tokens');
    expect(context.getAttribute('aria-label')).toContain('tokens');
    expect(cost.getAttribute('aria-label')).toContain('Reported cost');
  });

  it('handles context-only, cost-only, and absent usage independently', () => {
    fixture.componentRef.setInput('usage', { used: 1200, size: 200000, cost_amount: null, cost_currency: null });
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelector('.context-badge')).toBeTruthy();
    expect(fixture.nativeElement.querySelector('.cost-badge')).toBeNull();

    fixture.componentRef.setInput('usage', { used: 0, size: 0, cost_amount: 0.5, cost_currency: 'EUR' });
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelector('.context-badge')).toBeNull();
    expect((fixture.nativeElement.querySelector('.cost-badge') as HTMLElement).textContent.trim()).toBe('0.500 EUR');

    fixture.componentRef.setInput('usage', null);
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelector('.context-badge')).toBeNull();
    expect(fixture.nativeElement.querySelector('.cost-badge')).toBeNull();
  });

  it('copies the branch name when the workspace tag is clicked', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    const branch = 'batey/chat/chat-1';
    fixture.componentRef.setInput('chat', { ...mockChat, workspace: {
      mode: 'managed_worktree', branch, base_commit: 'abcdef1234567890',
    } });
    fixture.detectChanges();

    const badge = fixture.nativeElement.querySelector('.workspace-badge') as HTMLButtonElement;
    badge.click();
    await fixture.whenStable();
    expect(writeText).toHaveBeenCalledWith(branch);
  });
});

describe('ChatHeaderComponent working duration', () => {
  let fixture: ComponentFixture<ChatHeaderComponent>;

  beforeEach(async () => {
    vi.useFakeTimers({ now: new Date('2026-09-13T12:00:00Z') });
    await TestBed.configureTestingModule({
      imports: [ChatHeaderComponent],
      providers: [{
        provide: AppStateService,
        useValue: {
          isMobileDrawerOpen: () => false,
          setMobileDrawerOpen: () => undefined,
          chatActivity: () => 'working',
          chatTurnStartedAt: () => '2026-09-13T11:58:36Z',
          archiveChat: async () => undefined,
        },
      }],
    }).compileComponents();
    fixture = TestBed.createComponent(ChatHeaderComponent);
    fixture.componentRef.setInput('chat', {
      id: 'chat-1', project_id: 'proj-1', agent: 'codex', title: 'Active work',
      created_at: '2026-09-13T11:00:00Z', updated_at: '2026-09-13T12:00:00Z',
      archived: false, permission_policy: 'ask', config_values: {}, turn_state: 'PROMPTING',
    });
    fixture.detectChanges();
  });

  afterEach(() => vi.useRealTimers());

  it('shows and advances the working duration in the active header', () => {
    expect(fixture.nativeElement.querySelector('.chat-status').textContent.trim()).toBe('Working… 1m 24s');
    vi.advanceTimersByTime(1000);
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelector('.chat-status').textContent.trim()).toBe('Working… 1m 25s');
    expect(fixture.nativeElement.querySelector('.chat-status').getAttribute('aria-label')).toBe('Working… 1m 25s');
  });
});
