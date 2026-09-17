import { ComponentFixture, TestBed } from '@angular/core/testing';
import { MAT_DIALOG_DATA, MatDialogRef } from '@angular/material/dialog';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ApiService } from '../../core/api/api.service';
import type { Chat, TerminalTaskDetails, TerminalTaskSummary } from '../../core/api/types';
import { TerminalTaskDialogComponent } from './terminal-task-dialog';

describe('TerminalTaskDialogComponent', () => {
  let fixture: ComponentFixture<TerminalTaskDialogComponent>;

  const chat: Chat = {
    id: 'chat-1',
    project_id: 'proj-1',
    agent: 'antigravity',
    title: 'Test Chat',
    created_at: '2026-09-14T00:00:00Z',
    updated_at: '2026-09-14T00:00:00Z',
    archived: false,
    permission_policy: 'ask',
    config_values: {},
  };

  const tasks: TerminalTaskSummary[] = [
    {
      id: 'task-1',
      chat_id: 'chat-1',
      command: 'cargo test',
      cwd: '/workspace',
      state: 'running',
      started_at: '2026-09-14T00:00:00Z',
    },
    {
      id: 'task-2',
      chat_id: 'chat-1',
      command: 'npm run build',
      cwd: '/workspace/frontend',
      state: 'completed',
      exit_code: 0,
      started_at: '2026-09-14T00:00:00Z',
      completed_at: '2026-09-14T00:01:00Z',
    },
  ];

  const task1Details: TerminalTaskDetails = {
    ...tasks[0],
    output: 'running 5 tests...\ntest result: ok',
    truncated: false,
  };

  const mockApi = {
    fetchChatTasks: vi.fn(async () => tasks),
    fetchChatTask: vi.fn(async (_chatId: string, taskId: string) => {
      if (taskId === 'task-1') return task1Details;
      return { ...tasks[1], output: 'built successfully', truncated: false };
    }),
    stopChatTask: vi.fn(async () => undefined),
  };

  beforeEach(async () => {
    vi.clearAllMocks();
    await TestBed.configureTestingModule({
      imports: [TerminalTaskDialogComponent],
      providers: [
        { provide: MAT_DIALOG_DATA, useValue: chat },
        { provide: MatDialogRef, useValue: { close: vi.fn() } },
        { provide: ApiService, useValue: mockApi },
      ],
    }).compileComponents();

    fixture = TestBed.createComponent(TerminalTaskDialogComponent);
    fixture.detectChanges();
    await fixture.componentInstance.loadTasks();
    fixture.detectChanges();
  });

  it('loads tasks on init and renders them in the list', () => {
    expect(mockApi.fetchChatTasks).toHaveBeenCalledWith('chat-1');
    const items = fixture.nativeElement.querySelectorAll('.task-item');
    expect(items.length).toBe(2);
    expect(items[0].textContent).toContain('cargo test');
    expect(items[1].textContent).toContain('npm run build');
  });

  it('selects the running task by default and renders output', () => {
    expect(mockApi.fetchChatTask).toHaveBeenCalledWith('chat-1', 'task-1');
    const output = fixture.nativeElement.querySelector('.terminal-output');
    expect(output).toBeTruthy();
    expect(output.textContent).toContain('running 5 tests...');
    expect(fixture.nativeElement.querySelector('.stop-task')).toBeTruthy();
    expect(fixture.nativeElement.querySelector('.stop-task mat-icon')?.textContent.trim()).toBe('stop');
  });

  it('keeps Stop task disabled and reports stopping while the request is in flight', async () => {
    let resolveStop!: () => void;
    mockApi.stopChatTask.mockImplementationOnce(() => new Promise<undefined>((resolve) => { resolveStop = () => resolve(undefined); }));
    const stopPromise = fixture.componentInstance.stopTask('task-1');
    fixture.detectChanges();

    const stopBtn = fixture.nativeElement.querySelector('.stop-task') as HTMLButtonElement;
    expect(stopBtn).toBeTruthy();
    expect(stopBtn.disabled).toBe(true);
    expect(stopBtn.textContent).toContain('Stopping…');

    resolveStop();
    await stopPromise;
    fixture.detectChanges();
    expect(mockApi.stopChatTask).toHaveBeenCalledWith('chat-1', 'task-1');
  });

  it('does not show the destructive action for completed or stopped tasks', async () => {
    const items = fixture.nativeElement.querySelectorAll('.task-item') as NodeListOf<HTMLButtonElement>;
    items[1].click();
    await fixture.whenStable();
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelector('.stop-task')).toBeNull();

    fixture.componentInstance.selectedTaskDetails.set({ ...task1Details, state: 'stopped' });
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelector('.stop-task')).toBeNull();
  });

  it('formats the selected task start time for local display', () => {
    const time = fixture.nativeElement.querySelector('.detail-item time') as HTMLElement;
    expect(time.textContent).not.toContain('T');
    expect(time.textContent).not.toContain('.000Z');
    expect(time.textContent).toBe(fixture.componentInstance.formatDateTime(task1Details.started_at));
  });

  it('keeps the dialog close affordance accessible', () => {
    expect(fixture.nativeElement.querySelector('[aria-label="Close terminal tasks dialog"]')).toBeTruthy();
  });

  it('hides Stop for running observational tasks and shows the agent-managed note', () => {
    const observed: TerminalTaskDetails = {
      ...task1Details,
      id: 'task-obs',
      state: 'running',
      managed: false,
    };
    fixture.componentInstance.selectedTaskDetails.set(observed);
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelector('.stop-task')).toBeNull();
    const note = fixture.nativeElement.querySelector('.agent-managed-note');
    expect(note).toBeTruthy();
    expect(note.textContent).toContain('Agent-managed');
    expect(fixture.componentInstance.isTaskStoppable(observed)).toBe(false);
  });

  it('treats tasks without a managed flag as stoppable for backwards compatibility', () => {
    const legacy = { ...task1Details, state: 'running' as const };
    delete (legacy as Partial<TerminalTaskDetails>).managed;
    expect(fixture.componentInstance.isTaskStoppable(legacy)).toBe(true);
    expect(fixture.componentInstance.isTaskStoppable({ ...legacy, managed: true })).toBe(true);
    expect(fixture.componentInstance.isTaskStoppable({ ...legacy, managed: false })).toBe(false);
    expect(fixture.componentInstance.isTaskStoppable({ ...legacy, state: 'completed' as const, managed: false })).toBe(false);
    expect(fixture.componentInstance.isTaskStoppable(null)).toBe(false);
  });
});
