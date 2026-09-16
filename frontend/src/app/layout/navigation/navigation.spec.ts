import { ComponentFixture, TestBed } from '@angular/core/testing';
import { describe, expect, it, beforeEach, afterEach, vi } from 'vitest';
import { NavigationComponent } from './navigation';
import { AppStateService } from '../../state/app-state.service';
import { ThemeService } from '../../core/theme.service';
import { Router, provideRouter } from '@angular/router';
import { MatDialog } from '@angular/material/dialog';
import { signal } from '@angular/core';
import { APP_VERSION } from '../../version';

describe('NavigationComponent DOM check', () => {
  let fixture: ComponentFixture<NavigationComponent>;

  beforeEach(async () => {
    const mockState = {
      activeProjectId: signal('proj-1'),
      activeProject: signal({ id: 'proj-1', name: 'batey', path: '/home/dev/projects/batey' }),
      projects: signal([]),
      agents: signal(['claude', 'codex', 'opencode']),
      chatsByProject: signal({
        'proj-1': [
          {
            id: 'chat-1',
            project_id: 'proj-1',
            agent: 'claude',
            title: 'Review the WebSocket replay path',
            updated_at: '2026-09-13T12:03:00Z',
            process_state: 'RUNNING',
            turn_state: 'IDLE',
            archived: false,
          },
          {
            id: 'chat-2',
            project_id: 'proj-1',
            agent: 'codex',
            title: 'Draft the migration plan',
            updated_at: '2026-09-13T12:02:00Z',
            process_state: 'STOPPED',
            turn_state: 'IDLE',
            archived: false,
          },
          {
            id: 'chat-3',
            project_id: 'proj-1',
            agent: 'opencode',
            title: 'Summarize the router guard',
            updated_at: '2026-09-13T12:01:00Z',
            process_state: 'DEAD',
            turn_state: 'IDLE',
            archived: false,
          },
        ],
      }),
      showArchived: signal(false),
      activeChatId: signal('chat-1'),
      isMobileDrawerOpen: signal(false),
      wsStatus: signal('connected'),
      wsError: signal(''),
      chatActivity: (chatId: string) =>
        chatId === 'chat-1' ? 'working' : chatId === 'chat-2' ? 'waiting' : 'error',
      chatTurnStartedAt: () => null,
    };

    await TestBed.configureTestingModule({
      imports: [NavigationComponent],
      providers: [
        { provide: AppStateService, useValue: mockState },
        { provide: ThemeService, useValue: { label: () => 'Dark', cycle: () => {}, icon: () => 'dark_mode' } },
        provideRouter([]),
        { provide: MatDialog, useValue: { open: () => {} } },
      ],
    }).compileComponents();

    fixture = TestBed.createComponent(NavigationComponent);
    fixture.detectChanges();
  });

  it('renders chat items with title and capitalized agent badge and no process state', () => {
    const title = fixture.nativeElement.querySelector('[matListItemTitle]');
    expect(title).toBeTruthy();
    expect(title.textContent).toContain('Review the WebSocket replay path');

    const badges = Array.from(fixture.nativeElement.querySelectorAll('.agent-badge')).map(
      (badge: unknown) => (badge as Element).textContent?.trim(),
    );
    expect(badges).toEqual(['Claude', 'Codex', 'Opencode']);
    const firstBadge = fixture.nativeElement.querySelector('.agent-badge');
    expect(firstBadge.querySelector('mat-icon')).toBeNull();

    const chatList = fixture.nativeElement.querySelector('mat-nav-list');
    expect(chatList).toBeTruthy();
    expect(chatList.querySelector('.status-dot')).toBeNull();
    expect(chatList.querySelector('.status-slot')).toBeNull();

    const text = chatList.textContent as string;
    for (const state of ['STARTING', 'RUNNING', 'STOPPED', 'DEAD', 'Starting', 'Running', 'Stopped', 'Dead']) {
      expect(text).not.toContain(state);
    }

    const labels = Array.from(fixture.nativeElement.querySelectorAll('a[mat-list-item]'))
      .map((item: unknown) => (item as Element).getAttribute('aria-label') ?? '');
    expect(labels).toHaveLength(3);
    for (const label of labels) {
      expect(label).toMatch(/Open chat/);
      expect(label).not.toMatch(/process/i);
      expect(label).not.toMatch(/starting|running|stopped|dead/i);
    }
  });

  it('offers a first-class agents management entry', () => {
    const link = fixture.nativeElement.querySelector('a[href="/agents"]') as HTMLAnchorElement;
    expect(link).toBeTruthy();
    expect(link.getAttribute('aria-label')).toBe('Manage agents');
  });

  it('shows the version from the frontend package metadata', () => {
    const version = fixture.nativeElement.querySelector('.version');
    expect(version?.textContent?.trim()).toBe(`v${APP_VERSION}`);
  });

  it('capitalizes agent names for display only', () => {
    expect(fixture.componentInstance.agentLabel('claude')).toBe('Claude');
    expect(fixture.componentInstance.agentLabel('codex')).toBe('Codex');
    expect(fixture.componentInstance.agentLabel('antigravity')).toBe('Antigravity');
    expect(fixture.componentInstance.agentLabel('opencode')).toBe('Opencode');
  });

  it('shows the shared activity badge in every chat row with an accessible label', () => {
    const badges = Array.from(
      fixture.nativeElement.querySelectorAll('hub-chat-status-badge .chat-status'),
    ).map((badge: unknown) => (badge as Element).textContent?.trim());
    expect(badges).toEqual(['Working…', 'Waiting for you', 'Error']);

    const labels = Array.from(fixture.nativeElement.querySelectorAll('a[mat-list-item]'))
      .map((item: unknown) => (item as Element).getAttribute('aria-label') ?? '');
    expect(labels).toEqual([
      'Open chat Review the WebSocket replay path, Working…',
      'Open chat Draft the migration plan, Waiting for you',
      'Open chat Summarize the router guard, Error',
    ]);
  });

  it('renders chat actions for every sidebar thread', () => {
    const actionButtons = fixture.nativeElement.querySelectorAll('.chat-actions-button');
    expect(actionButtons).toHaveLength(3);
    expect(actionButtons[0].getAttribute('aria-label')).toBe(
      'Actions for Review the WebSocket replay path',
    );
    (actionButtons[0] as HTMLButtonElement).click();
    fixture.detectChanges();
    expect(document.body.textContent).toContain('Rename chat');
    expect(document.body.textContent).toContain('Archive chat');
    expect(document.body.textContent).toContain('Delete chat');
  });
});

describe('NavigationComponent project switching', () => {
  const projects = [
    { id: 'p1', name: 'Batey', path: '/repos/batey' },
    { id: 'p2', name: 'Proj Two', path: '/repos/two' },
    { id: 'p3', name: 'Empty Project', path: '/repos/empty' },
  ];

  function makeChat(id: string, updatedAt: string, overrides: Record<string, unknown> = {}) {
    return {
      id,
      project_id: 'p2',
      agent: 'codex',
      title: `Chat ${id}`,
      archived: false,
      updated_at: updatedAt,
      ...overrides,
    };
  }

  function baseState() {
    return {
      activeProjectId: signal('p1'),
      activeProject: signal(projects[0]),
      projects: signal(projects),
      agents: signal(['codex']),
      chatsByProject: signal({
        p1: [makeChat('chat-1', '2026-01-01T00:00:00Z', { project_id: 'p1' })],
        p2: [makeChat('chat-old', '2026-01-02T00:00:00Z'), makeChat('chat-new', '2026-06-01T00:00:00Z')],
      }),
      showArchived: signal(false),
      activeChatId: signal('chat-1'),
      isMobileDrawerOpen: signal(false),
      wsStatus: signal('connected'),
      wsError: signal(''),
      loadChats: vi.fn(async () => {}),
      chatActivity: () => 'idle',
      chatTurnStartedAt: () => null,
    };
  }

  async function renderNavigation(state: Record<string, unknown>): Promise<ComponentFixture<NavigationComponent>> {
    await TestBed.configureTestingModule({
      imports: [NavigationComponent],
      providers: [
        { provide: AppStateService, useValue: state },
        { provide: ThemeService, useValue: { label: () => 'Dark', cycle: () => {}, icon: () => 'dark_mode' } },
        provideRouter([]),
        { provide: MatDialog, useValue: { open: () => {} } },
      ],
    }).compileComponents();

    const fixture = TestBed.createComponent(NavigationComponent);
    fixture.detectChanges();
    return fixture;
  }

  async function openProjectMenu(fixture: ComponentFixture<NavigationComponent>): Promise<HTMLElement[]> {
    (fixture.nativeElement as HTMLElement).querySelector<HTMLButtonElement>('.project-switcher')!.click();
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.detectChanges();
    return Array.from(document.querySelectorAll<HTMLElement>('.hub-project-menu .project-menu-item'));
  }

  function itemByText(items: HTMLElement[], text: string): HTMLElement {
    const item = items.find((candidate) => candidate.textContent?.includes(text));
    if (!item) throw new Error(`Menu item not found: ${text}`);
    return item;
  }

  it('opens the latest chat of the chosen project inside the chat view', async () => {
    const state = baseState();
    const fixture = await renderNavigation(state);
    const navigate = vi.spyOn(TestBed.inject(Router), 'navigate').mockResolvedValue(true);

    const items = await openProjectMenu(fixture);
    itemByText(items, 'Proj Two').click();
    await fixture.whenStable();

    expect(navigate).toHaveBeenCalledWith(['/projects', 'p2', 'chats', 'chat-new']);
  });

  it('opens the project page when the chosen project has no chats', async () => {
    const state = baseState();
    const fixture = await renderNavigation(state);
    const navigate = vi.spyOn(TestBed.inject(Router), 'navigate').mockResolvedValue(true);

    const items = await openProjectMenu(fixture);
    itemByText(items, 'Empty Project').click();
    await fixture.whenStable();

    expect(state.loadChats).toHaveBeenCalledWith('p3');
    expect(navigate).toHaveBeenCalledWith(['/projects', 'p3']);
  });

  it('keeps the current chat when the current project is chosen again', async () => {
    const state = baseState();
    const fixture = await renderNavigation(state);
    const navigate = vi.spyOn(TestBed.inject(Router), 'navigate').mockResolvedValue(true);

    const items = await openProjectMenu(fixture);
    itemByText(items, 'Batey').click();
    await fixture.whenStable();

    expect(navigate).not.toHaveBeenCalled();
  });
});

describe('NavigationComponent working duration', () => {
  let fixture: ComponentFixture<NavigationComponent>;

  beforeEach(async () => {
    vi.useFakeTimers({ now: new Date('2026-09-13T12:00:00Z') });
    const chat = {
      id: 'chat-working', project_id: 'proj-1', agent: 'codex', title: 'Active work',
      created_at: '2026-09-13T11:00:00Z', updated_at: '2026-09-13T12:00:00Z',
      archived: false, permission_policy: 'ask', config_values: {}, turn_state: 'PROMPTING',
    };
    const state = {
      activeProjectId: signal('proj-1'),
      activeProject: signal({ id: 'proj-1', name: 'Batey', path: '/work' }),
      projects: signal([]),
      chatsByProject: signal({ 'proj-1': [chat] }),
      showArchived: signal(false),
      activeChatId: signal('chat-working'),
      isMobileDrawerOpen: signal(false),
      wsStatus: signal('connected'),
      wsError: signal(''),
      chatActivity: () => 'working',
      chatTurnStartedAt: () => '2026-09-13T11:58:36Z',
    };
    await TestBed.configureTestingModule({
      imports: [NavigationComponent],
      providers: [
        { provide: AppStateService, useValue: state },
        { provide: ThemeService, useValue: { label: () => 'Dark', cycle: () => {}, icon: () => 'dark_mode' } },
        provideRouter([]),
        { provide: MatDialog, useValue: { open: () => {} } },
      ],
    }).compileComponents();
    fixture = TestBed.createComponent(NavigationComponent);
    fixture.detectChanges();
  });

  afterEach(() => vi.useRealTimers());

  it('shows and advances the working duration in the sidebar status', () => {
    expect(fixture.nativeElement.querySelector('.chat-status').textContent.trim()).toBe('Working… 1m 24s');
    vi.advanceTimersByTime(1000);
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelector('.chat-status').textContent.trim()).toBe('Working… 1m 25s');
    expect(fixture.nativeElement.querySelector('a[mat-list-item]').getAttribute('aria-label'))
      .toContain('Working… 1m 25s');
  });
});
