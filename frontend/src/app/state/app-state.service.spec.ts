import { TestBed } from '@angular/core/testing';
import { provideRouter } from '@angular/router';
import { signal } from '@angular/core';
import { Subject } from 'rxjs';
import { describe, expect, it, beforeEach, vi } from 'vitest';
import { ApiService } from '../core/api/api.service';
import type { Chat, Project, SessionEvent } from '../core/api/types';
import { EventSocketService } from '../core/event-socket.service';
import { AppStateService } from './app-state.service';

describe('AppStateService', () => {
  let state: AppStateService;
  let projects: Project[];
  let chats: Chat[];
  let resumeCalls: number;
  let permissionCalls: Array<{ chatId: string; requestId: string; optionId: string }>;
  let events: Subject<SessionEvent>;
  let replayGaps: Subject<void>;

  beforeEach(() => {
    projects = [{ id: 'project-1', name: 'Batey', path: '/work', created_at: '2026-01-01', updated_at: '2026-01-01', chat_count: 1 }];
    chats = [{ id: 'chat-1', project_id: 'project-1', agent: 'codex', title: 'First chat', created_at: '2026-01-01', updated_at: '2026-01-01', archived: false, permission_policy: 'ask', config_values: {}, process_state: 'STOPPED', turn_state: 'IDLE' }];
    resumeCalls = 0;
    permissionCalls = [];
    events = new Subject<SessionEvent>();
    replayGaps = new Subject<void>();
    const api = {
      fetchProjects: async () => [...projects], fetchAgents: async () => [{ id: 'codex', display_name: 'Codex', source: 'builtin', availability: 'available', metadata: null }], fetchChats: async () => [...chats],
      createProject: async (name: string, path: string) => ({ id: 'project-2', name, path, created_at: 'now', updated_at: 'now', chat_count: 0 }),
      editProject: async (id: string, name: string, path: string) => ({ ...projects[0], id, name, path }),
      deleteProject: async () => undefined,
      createChat: async (_projectId: string, agent: string) => {
        const created = { ...chats[0], id: 'chat-2', agent, title: 'New chat 1' };
        chats = [...chats, created];
        return created;
      }, fetchChatConfig: async () => [],
      resumeChat: async () => { resumeCalls++; return { ...chats[0], process_state: 'RUNNING' as const }; },
      editChat: async (id: string, edit: Partial<Chat>) => ({ ...(chats.find((chat) => chat.id === id) ?? chats[0]), ...edit }),
      stopChat: async () => undefined, cancelChat: async () => undefined, promptChat: async () => undefined,
      respondPermission: async (chatId: string, requestId: string, optionId: string) => { permissionCalls.push({ chatId, requestId, optionId }); }, setChatConfig: async () => [], cloneProject: async () => projects[0],
      authorizeChatEnvironment: async () => undefined,
      forgetProjectEnvrcGrant: async () => undefined,
    } as unknown as ApiService;
    const socket = { status: signal<'disconnected'>('disconnected'), events, replayGaps, connect: vi.fn() };
    TestBed.configureTestingModule({ providers: [{ provide: ApiService, useValue: api }, { provide: EventSocketService, useValue: socket }, provideRouter([])] });
    state = TestBed.inject(AppStateService);
  });

  it('keeps simultaneous reconnects single-flight and gates on config', async () => {
    await state.loadProjects();
    await state.loadChats('project-1');
    const first = state.connectChat('chat-1');
    const second = state.connectChat('chat-1');
    expect(first).toBe(second);
    await Promise.all([first, second]);
    expect(resumeCalls).toBe(1);
    expect(state.configLoadedByChat()['chat-1']).toBe(true);
    expect(state.findChat('chat-1')?.process_state).toBe('RUNNING');
  });

  it('updates project and chat signals through lifecycle operations', async () => {
    await state.loadProjects();
    const created = await state.createProject('Second', '/second');
    expect(state.projects().map((project) => project.name)).toContain('Second');
    await state.editProject(created.id, 'Renamed', '/renamed');
    expect(state.projects().find((project) => project.id === created.id)?.name).toBe('Renamed');
    await state.deleteProject(created.id);
    expect(state.projects().some((project) => project.id === created.id)).toBe(false);
  });

  it('removes a project and its cached chats immediately after cascading deletion', async () => {
    await state.loadProjects();
    await state.loadChats('project-1');
    expect(state.chatsByProject()['project-1']).toHaveLength(1);
    await state.deleteProject('project-1');
    expect(state.projects().some((project) => project.id === 'project-1')).toBe(false);
    expect(state.chatsByProject()['project-1']).toBeUndefined();
  });

  it('creates chats and applies streamed permission and process events', async () => {
    await state.loadProjects();
    await state.loadChats('project-1');
    state.activeProjectId.set('project-1');
    state.activeChatId.set('chat-1');

    const created = await state.createChat('project-1', 'claude');
    expect(created).toMatchObject({ id: 'chat-2', agent: 'claude', title: 'New chat 1' });
    expect(state.chatsByProject()['project-1']).toHaveLength(2);

    events.next({ seq: 1, session_id: 'chat-1', agent: 'codex', timestamp: '2026-09-13T12:00:00Z', payload: { type: 'permission_request', id: 'permission-1', method: 'terminal/run_command', description: 'Run tests' } });
    expect(state.activeReducer().items()[0]).toMatchObject({ type: 'turn' });
    await state.respondPermission('chat-1', 'permission-1', 'allow-once');
    expect(permissionCalls).toEqual([{ chatId: 'chat-1', requestId: 'permission-1', optionId: 'allow-once' }]);

    events.next({ seq: 2, session_id: 'chat-1', agent: 'codex', timestamp: '2026-09-13T12:00:01Z', payload: { type: 'permission_response', id: 'permission-1', option_id: 'allow-once' } });
    const turn = state.activeReducer().items()[0];
    expect(turn).toMatchObject({ type: 'turn' });
    if (turn.type === 'turn') expect(turn.entries[0]).toMatchObject({ responded: true, decision: 'allow-once' });

    events.next({ seq: 3, session_id: 'chat-1', agent: 'codex', timestamp: '2026-09-13T12:00:02Z', payload: { type: 'state_change', process: 'RUNNING', turn: 'IDLE' } });
    expect(state.findChat('chat-1')?.process_state).toBe('RUNNING');
  });

  it('applies a successful rename immediately while the chat is working', async () => {
    chats[0] = { ...chats[0], turn_state: 'PROMPTING' };
    await state.loadChats('project-1');

    await state.renameChat('chat-1', 'Renamed while working');

    expect(state.findChat('chat-1')).toMatchObject({
      title: 'Renamed while working',
      turn_state: 'PROMPTING',
    });
    expect(state.chatsByProject()['project-1'][0].title).toBe('Renamed while working');
  });

  it('refreshes project and chat metadata after a metadata event', async () => {
    await state.loadProjects();
    await state.loadChats('project-1');
    state.activeProjectId.set('project-1');
    projects[0] = { ...projects[0], name: 'Renamed by ACP' };

    events.next({ seq: 1, session_id: 'chat-1', agent: 'codex', timestamp: '2026-09-13T12:00:00Z', payload: { type: 'metadata_changed' } });
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(state.activeProject()?.name).toBe('Renamed by ACP');
  });

  it('clears reduced event state when the server reports a replay gap', async () => {
    await state.loadChats('project-1');
    events.next({ seq: 1, session_id: 'chat-1', agent: 'codex', timestamp: '2026-09-13T12:00:00Z', payload: { type: 'user_message', text: 'partial' } });
    expect(state.reducersByChat()['chat-1']).toBeDefined();

    replayGaps.next();

    expect(state.reducersByChat()).toEqual({});
  });

  it('remembering a chat environment immediately marks its project as remembered', async () => {
    await state.loadProjects();
    await state.loadChats('project-1');

    await state.authorizeChatEnvironment('chat-1', true);

    expect(state.projects().find((project) => project.id === 'project-1')).toMatchObject({
      envrc_remembered: true,
    });
  });

  it('a plain workspace allow never marks the project as remembered', async () => {
    await state.loadProjects();
    await state.loadChats('project-1');

    await state.authorizeChatEnvironment('chat-1', false);

    expect(state.projects().find((project) => project.id === 'project-1')?.envrc_remembered).toBeFalsy();
  });
});
