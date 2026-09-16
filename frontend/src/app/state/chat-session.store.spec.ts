import { TestBed } from '@angular/core/testing';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ApiError, ApiService } from '../core/api/api.service';
import { EventSocketService } from '../core/event-socket.service';
import type { Chat, SessionEvent } from '../core/api/types';
import { ChatSessionStore } from './chat-session.store';

describe('ChatSessionStore', () => {
  const chat: Chat = {
    id: 'chat-1',
    project_id: 'project-1',
    agent: 'codex',
    title: 'Test chat',
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
    archived: false,
    permission_policy: 'ask',
    config_values: {},
    process_state: 'STOPPED',
    turn_state: 'IDLE',
  };

  let store: ChatSessionStore;
  let socket: { waitForBaseline: ReturnType<typeof vi.fn> };
  let api: {
    fetchChats: ReturnType<typeof vi.fn>;
    resumeChat: ReturnType<typeof vi.fn>;
    fetchChatConfig: ReturnType<typeof vi.fn>;
    fetchChatHistory: ReturnType<typeof vi.fn>;
    clearSavedConfig: ReturnType<typeof vi.fn>;
    deleteChat: ReturnType<typeof vi.fn>;
    promptChat: ReturnType<typeof vi.fn>;
  };

  beforeEach(() => {
    api = {
      fetchChats: vi.fn(async () => [chat]),
      resumeChat: vi.fn(async () => ({ ...chat, process_state: 'RUNNING' })),
      fetchChatConfig: vi.fn(async () => [
        {
          id: 'model',
          name: 'Model',
          type: 'select',
          currentValue: 'gpt-5',
          options: [{ value: 'gpt-5', name: 'GPT-5' }],
        },
      ]),
      fetchChatHistory: vi.fn(async () => ({ events: [], next_cursor: null, has_older: false })),
      clearSavedConfig: vi.fn(async () => undefined),
      deleteChat: vi.fn(async () => undefined),
      promptChat: vi.fn(async () => undefined),
    };
    socket = { waitForBaseline: vi.fn(async () => 100) };
    TestBed.configureTestingModule({
      providers: [
        { provide: ApiService, useValue: api },
        { provide: EventSocketService, useValue: socket },
      ],
    });
    store = TestBed.inject(ChatSessionStore);
  });

  it('deduplicates concurrent chat loads and runtime config initialization', async () => {
    const firstLoad = store.loadChats('project-1');
    const secondLoad = store.loadChats('project-1');
    expect(secondLoad).toBe(firstLoad);
    await firstLoad;

    const firstConfig = store.loadChatConfig('chat-1');
    const secondConfig = store.loadChatConfig('chat-1');
    expect(secondConfig).toBe(firstConfig);
    await firstConfig;

    expect(api.fetchChats).toHaveBeenCalledOnce();
    expect(api.fetchChatConfig).toHaveBeenCalledOnce();
    expect(store.configLoadedByChat()['chat-1']).toBe(true);
  });

  it('retains an event created before the baseline across delayed initial history and deduplicates overlap', async () => {
    store.chatsByProject.set({ 'project-1': [chat] });
    let resolvePage: ((page: { events: SessionEvent[]; next_cursor: number | null; has_older: boolean }) => void) | undefined;
    let resolveBaseline: ((sequence: number) => void) | undefined;
    socket.waitForBaseline.mockImplementationOnce(() => new Promise((resolve) => {
      resolveBaseline = resolve;
    }));
    api.fetchChatHistory.mockImplementationOnce(() => new Promise((resolve) => { resolvePage = resolve; }));

    const loading = store.loadChatHistory('chat-1');
    await Promise.resolve();
    expect(api.fetchChatHistory).not.toHaveBeenCalled();
    store.handleIncomingEvent({
      seq: 3,
      session_id: 'chat-1',
      agent: 'codex',
      timestamp: '2026-01-01T00:00:03Z',
      payload: { type: 'turn_complete', stop_reason: 'end_turn' },
    });
    resolveBaseline?.(3);
    await Promise.resolve();
    expect(api.fetchChatHistory).toHaveBeenCalledWith('chat-1', undefined, 3);
    resolvePage?.({
      events: [
        { seq: 1, session_id: 'chat-1', agent: 'codex', timestamp: '2026-01-01T00:00:01Z', payload: { type: 'user_message', text: 'Hello' } },
        { seq: 2, session_id: 'chat-1', agent: 'codex', timestamp: '2026-01-01T00:00:02Z', payload: { type: 'message_chunk', text: 'Answer' } },
        { seq: 3, session_id: 'chat-1', agent: 'codex', timestamp: '2026-01-01T00:00:03Z', payload: { type: 'turn_complete', stop_reason: 'end_turn' } },
      ],
      next_cursor: null,
      has_older: false,
    });
    await loading;

    const items = store.reducersByChat()['chat-1'].items();
    expect(items).toHaveLength(2);
    expect(items[0]).toMatchObject({ type: 'user_message', text: 'Hello' });
    expect(items[1]).toMatchObject({ type: 'turn', status: 'complete' });
  });

  it('waits for the fresh live baseline before requesting initial history', async () => {
    store.chatsByProject.set({ 'project-1': [chat] });
    let resolveBaseline: ((sequence: number) => void) | undefined;
    socket.waitForBaseline.mockImplementationOnce(() => new Promise((resolve) => {
      resolveBaseline = resolve;
    }));

    const loading = store.loadChatHistory('chat-1');
    await Promise.resolve();
    expect(api.fetchChatHistory).not.toHaveBeenCalled();

    resolveBaseline?.(23);
    await loading;
    expect(api.fetchChatHistory).toHaveBeenCalledWith('chat-1', undefined, 23);
  });

  it('keeps rendered history when an older page fails and retries from the same cursor', async () => {
    store.chatsByProject.set({ 'project-1': [chat] });
    api.fetchChatHistory
      .mockResolvedValueOnce({
        events: [{ seq: 10, session_id: 'chat-1', agent: 'codex', timestamp: '2026-01-01T00:00:10Z', payload: { type: 'user_message', text: 'Newest' } }],
        next_cursor: 10,
        has_older: true,
      })
      .mockRejectedValueOnce(new Error('timed out'))
      .mockResolvedValueOnce({
        events: [{ seq: 9, session_id: 'chat-1', agent: 'codex', timestamp: '2026-01-01T00:00:09Z', payload: { type: 'user_message', text: 'Older' } }],
        next_cursor: null,
        has_older: false,
      });

    await store.loadChatHistory('chat-1');
    await store.loadOlderHistory('chat-1');
    expect(store.reducersByChat()['chat-1'].items()[0]).toMatchObject({ text: 'Newest' });
    expect(store.historyErrors()['chat-1']).toBe('timed out');

    await store.retryHistory('chat-1');
    expect(api.fetchChatHistory).toHaveBeenLastCalledWith('chat-1', 10);
    expect(store.historyErrors()['chat-1']).toBeUndefined();
    expect(store.reducersByChat()['chat-1'].items().map((item) => item.type)).toEqual(['user_message', 'user_message']);
  });

  it('preserves the active turn start across reload replay and older history pages', async () => {
    store.chatsByProject.set({
      'project-1': [{
        ...chat,
        turn_state: 'PROMPTING',
        turn_started_at: '2026-01-01T00:00:00Z',
      }],
    });
    api.fetchChatHistory
      .mockResolvedValueOnce({
        events: [
          { seq: 2, session_id: 'chat-1', agent: 'codex', timestamp: '2026-01-01T00:00:05Z', payload: { type: 'state_change', process: 'RUNNING', turn: 'PROMPTING' } },
          { seq: 3, session_id: 'chat-1', agent: 'codex', timestamp: '2026-01-01T00:00:10Z', payload: { type: 'message_chunk', text: 'Working' } },
        ],
        next_cursor: 2,
        has_older: true,
      })
      .mockResolvedValueOnce({
        events: [{ seq: 1, session_id: 'chat-1', agent: 'codex', timestamp: '2026-01-01T00:00:00Z', payload: { type: 'user_message', text: 'Prompt' } }],
        next_cursor: null,
        has_older: false,
      });

    await store.loadChatHistory('chat-1');
    expect(store.chatTurnStartedAt('chat-1')).toBe('2026-01-01T00:00:00Z');
    await store.loadOlderHistory('chat-1');
    expect(store.chatTurnStartedAt('chat-1')).toBe('2026-01-01T00:00:00Z');
    expect(store.chatActivity('chat-1')).toBe('working');
  });

  it('treats process state as diagnostic only and allows sending prompts while stopped', async () => {
    const stoppedChat: Chat = { ...chat, process_state: 'STOPPED', turn_state: 'IDLE' };
    store.chatsByProject.set({ 'project-1': [stoppedChat] });

    await store.sendPrompt('chat-1', 'Hello agent');

    expect(api.promptChat).toHaveBeenCalledWith('chat-1', 'Hello agent');
    expect(api.resumeChat).not.toHaveBeenCalled();
    expect(store.findChat('chat-1')?.turn_state).toBe('PROMPTING');
  });

  it('reduces streamed entries and process changes into chat-owned state', () => {
    store.chatsByProject.set({ 'project-1': [chat] });
    const event = (seq: number, payload: SessionEvent['payload']): SessionEvent => ({
      seq,
      session_id: 'chat-1',
      agent: 'codex',
      timestamp: `2026-01-01T00:00:0${seq}Z`,
      payload,
    });

    store.handleIncomingEvent(event(1, {
      type: 'permission_request',
      id: 'permission-1',
      method: 'terminal/run_command',
      description: 'Run tests',
    }));
    store.handleIncomingEvent(event(2, {
      type: 'state_change',
      process: 'RUNNING',
      turn: 'PROMPTING',
    }));

    expect(store.reducersByChat()['chat-1'].items()[0]).toMatchObject({ type: 'turn' });
    expect(store.reducersByChat()['chat-1'].items()).toHaveLength(1);
    expect(store.findChat('chat-1')).toMatchObject({
      process_state: 'RUNNING',
      turn_state: 'PROMPTING',
    });
  });

  it('moves a chat to the newest position when a live user message arrives', () => {
    const older = { ...chat, id: 'chat-older', updated_at: '2026-01-01T00:00:00Z' };
    const newer = { ...chat, id: 'chat-newer', updated_at: '2026-01-02T00:00:00Z' };
    store.chatsByProject.set({ 'project-1': [newer, older] });
    store.handleIncomingEvent({
      seq: 1,
      session_id: 'chat-older',
      agent: 'codex',
      timestamp: '2026-01-03T00:00:00Z',
      payload: { type: 'user_message', text: 'Make this chat recent' },
    });

    expect(store.chatsByProject()['project-1'].map((candidate) => candidate.id))
      .toEqual(['chat-older', 'chat-newer']);
    expect(store.findChat('chat-older')?.updated_at).toBe('2026-01-03T00:00:00Z');
  });

  it('patches process state without adding a visible transcript item', () => {
    store.chatsByProject.set({ 'project-1': [chat] });
    const event = (seq: number, payload: SessionEvent['payload']): SessionEvent => ({
      seq,
      session_id: 'chat-1',
      agent: 'codex',
      timestamp: `2026-01-01T00:00:0${seq}Z`,
      payload,
    });

    const processes = ['STARTING', 'RUNNING', 'STOPPED', 'DEAD'] as const;
    processes.forEach((process, index) => {
      store.handleIncomingEvent(event(index + 1, {
        type: 'state_change',
        process,
        turn: 'IDLE',
      }));
    });

    expect(store.reducersByChat()['chat-1'].items()).toHaveLength(0);
    expect(store.findChat('chat-1')).toMatchObject({
      process_state: 'DEAD',
      turn_state: 'IDLE',
    });
  });

  it('maps a rejected saved model to the explicit reset path', async () => {
    store.chatsByProject.set({ 'project-1': [chat] });
    api.fetchChatConfig.mockRejectedValueOnce(
      new ApiError(409, 'Saved ACP option model could not be reapplied', 'saved_config_rejected', {
        option_id: 'model',
      }),
    );

    await expect(store.loadChatConfig('chat-1')).rejects.toThrow();
    expect(store.connectErrors()['chat-1']).toContain('could not be reapplied');
    expect(store.rejectedConfigByChat()['chat-1']).toBe('model');
  });

  it('keeps a transient config-application failure on the retry path', async () => {
    store.chatsByProject.set({ 'project-1': [chat] });
    api.fetchChatConfig.mockRejectedValueOnce(
      new ApiError(400, 'Failed to reapply saved ACP option model; retry to reconnect'),
    );

    await expect(store.loadChatConfig('chat-1')).rejects.toThrow();
    expect(store.connectErrors()['chat-1']).toContain('retry to reconnect');
    expect(store.rejectedConfigByChat()['chat-1']).toBeUndefined();
  });

  it('reset deletes only the explicitly rejected option then retries config initialization', async () => {
    store.chatsByProject.set({ 'project-1': [chat] });
    store.rejectedConfigByChat.set({ 'chat-1': 'model' });

    await store.resetRejectedConfig('chat-1');

    expect(api.clearSavedConfig).toHaveBeenCalledWith('chat-1', 'model');
    expect(store.rejectedConfigByChat()['chat-1']).toBeUndefined();
    expect(api.fetchChatConfig).toHaveBeenCalledWith('chat-1');
  });

  it('deleting a chat also deletes its stale rejected-config entry', async () => {
    store.chatsByProject.set({ 'project-1': [chat] });
    store.rejectedConfigByChat.set({ 'chat-1': 'model' });
    store.connectErrors.set({ 'chat-1': 'Saved ACP option model could not be reapplied' });

    await store.deleteChat('chat-1');

    expect(store.findChat('chat-1')).toBeNull();
    expect(store.rejectedConfigByChat()['chat-1']).toBeUndefined();
    expect(store.connectErrors()['chat-1']).toBeUndefined();
  });

  it('derives Idle for an ordinary completed chat', () => {
    store.chatsByProject.set({ 'project-1': [{ ...chat, turn_state: 'IDLE' }] });
    const event = (seq: number, payload: SessionEvent['payload']): SessionEvent => ({
      seq,
      session_id: 'chat-1',
      agent: 'codex',
      timestamp: `2026-01-01T00:00:0${seq}Z`,
      payload,
    });
    store.handleIncomingEvent(event(1, { type: 'user_message', text: 'Hello' }));
    store.handleIncomingEvent(event(2, { type: 'message_chunk', text: 'Hi' }));
    store.handleIncomingEvent(event(3, { type: 'turn_complete', stop_reason: 'end_turn' }));
    expect(store.chatActivity('chat-1')).toBe('idle');
  });

  it('derives Working while PROMPTING and Waiting while a permission is pending', () => {
    store.chatsByProject.set({ 'project-1': [{ ...chat, turn_state: 'PROMPTING' }] });
    expect(store.chatActivity('chat-1')).toBe('working');

    const event = (seq: number, payload: SessionEvent['payload']): SessionEvent => ({
      seq,
      session_id: 'chat-1',
      agent: 'codex',
      timestamp: `2026-01-01T00:00:0${seq}Z`,
      payload,
    });
    store.handleIncomingEvent(event(1, {
      type: 'permission_request',
      id: 'permission-1',
      method: 'fs/write_text_file',
      description: 'Write file',
    }));
    expect(store.chatActivity('chat-1')).toBe('waiting');

    store.handleIncomingEvent(event(2, { type: 'permission_response', id: 'permission-1', option_id: 'allow-once' }));
    expect(store.chatActivity('chat-1')).toBe('working');
  });

  it('derives Error from connection failures and failed turns, then clears on a later success', () => {
    store.chatsByProject.set({ 'project-1': [{ ...chat, turn_state: 'IDLE' }] });
    store.connectErrors.set({ 'chat-1': 'Failed to connect to agent' });
    expect(store.chatActivity('chat-1')).toBe('error');
    store.connectErrors.set({});

    const event = (seq: number, payload: SessionEvent['payload']): SessionEvent => ({
      seq,
      session_id: 'chat-1',
      agent: 'codex',
      timestamp: `2026-01-01T00:00:0${seq}Z`,
      payload,
    });
    store.handleIncomingEvent(event(1, { type: 'user_message', text: 'Break it' }));
    store.handleIncomingEvent(event(2, { type: 'thought_chunk', text: 'Trying' }));
    store.handleIncomingEvent(event(3, { type: 'error', message: 'boom' }));
    store.handleIncomingEvent(event(4, { type: 'turn_complete', stop_reason: 'error' }));
    expect(store.chatActivity('chat-1')).toBe('error');

    store.handleIncomingEvent(event(5, { type: 'user_message', text: 'Try again' }));
    store.chatsByProject.set({ 'project-1': [{ ...chat, turn_state: 'PROMPTING' }] });
    expect(store.chatActivity('chat-1')).toBe('working');

    store.handleIncomingEvent(event(6, { type: 'message_chunk', text: 'Fixed' }));
    store.handleIncomingEvent(event(7, { type: 'turn_complete', stop_reason: 'end_turn' }));
    store.chatsByProject.set({ 'project-1': [{ ...chat, turn_state: 'IDLE' }] });
    expect(store.chatActivity('chat-1')).toBe('idle');
  });

  it('never lets process state alone change the user-facing activity', () => {
    for (const process_state of ['STARTING', 'RUNNING', 'STOPPED', 'DEAD'] as const) {
      store.chatsByProject.set({ 'project-1': [{ ...chat, process_state, turn_state: 'IDLE' }] });
      expect(store.chatActivity('chat-1')).toBe('idle');
    }
    for (const process_state of ['STARTING', 'RUNNING', 'STOPPED', 'DEAD'] as const) {
      store.chatsByProject.set({ 'project-1': [{ ...chat, process_state, turn_state: 'PROMPTING' }] });
      expect(store.chatActivity('chat-1')).toBe('working');
    }
  });
});
