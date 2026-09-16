import { describe, expect, it } from 'vitest';
import { EventReducer } from './event-reducer';
import type { SessionEvent } from '../core/api/types';

const event = (seq: number, type: SessionEvent['payload']['type'], extra: Record<string, unknown> = {}): SessionEvent => ({
  seq, session_id: 'chat-1', agent: 'codex', timestamp: '2026-09-13T12:00:00Z', payload: { type, ...extra },
});

describe('EventReducer', () => {
  it('retains ordered rich blocks and message IDs through replay', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'message_chunk', {
      text: 'before', message_id: 'rich-1', content: [{ type: 'text', text: 'before' }],
    }));
    reducer.ingest(event(2, 'message_chunk', {
      text: '', message_id: 'rich-1', content: [{ type: 'resource_link', name: 'safe', uri: 'https://example.test' }],
    }));
    const entry = reducer.items()[0] as { type: string; entries: Array<{ messageId?: string; content?: unknown[] }> };
    expect(entry.entries[0].messageId).toBe('rich-1');
    expect(entry.entries[0].content).toEqual([
      { type: 'text', text: 'before' },
      { type: 'resource_link', name: 'safe', uri: 'https://example.test' },
    ]);
  });
  it('aggregates streamed messages and completes the turn', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'user_message', { text: 'Inspect this' }));
    reducer.ingest(event(2, 'message_chunk', { text: 'Hello ' }));
    reducer.ingest(event(3, 'message_chunk', { text: 'world' }));
    reducer.ingest(event(4, 'turn_complete', { stop_reason: 'end_turn' }));
    expect(reducer.items()).toHaveLength(2);
    expect(reducer.items()[1]).toMatchObject({ type: 'turn', status: 'complete', stopReason: 'end_turn' });
    expect((reducer.items()[1] as { entries: Array<{ text: string }> }).entries[0].text).toBe('Hello world');
  });

  it('deduplicates replayed sequence numbers and resolves permissions', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'permission_request', { id: 'permission-1', method: 'execute_command', description: 'Run tests' }));
    reducer.ingest(event(1, 'permission_request', { id: 'permission-1', method: 'execute_command', description: 'Duplicate' }));
    reducer.ingest(event(2, 'permission_response', { id: 'permission-1', option_id: 'allow-once' }));
    expect(reducer.items()).toHaveLength(1);
    expect((reducer.items()[0] as { entries: Array<{ responded?: boolean; decision?: string }> }).entries[0]).toMatchObject({ responded: true, decision: 'allow-once' });
  });

  it('updates tool output and preserves thought entries', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'thought_chunk', { text: 'Inspecting the repository' }));
    reducer.ingest(event(2, 'tool_call', { toolCallId: 'tool-1', title: 'List files', status: 'running' }));
    reducer.ingest(event(3, 'tool_call_update', { toolCallId: 'tool-1', status: 'completed', output: 'README.md' }));

    expect(reducer.items()[0]).toMatchObject({ type: 'turn', status: 'in_progress' });
    expect((reducer.items()[0] as unknown as { entries: Array<Record<string, unknown>> }).entries).toEqual([
      expect.objectContaining({ type: 'thought_chunk', text: 'Inspecting the repository' }),
      expect.objectContaining({ type: 'tool_call', toolCallId: 'tool-1', status: 'completed', output: 'README.md' }),
    ]);
  });

  it('replaces the active plan without adding transcript items for process state changes', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'plan', { entries: [{ content: 'Run tests', status: 'in_progress' }] }));
    reducer.ingest(event(2, 'plan', { entries: [{ content: 'Run tests', status: 'completed' }] }));
    reducer.ingest(event(3, 'turn_complete', { stop_reason: 'end_turn' }));

    expect(reducer.items()[0]).toMatchObject({ type: 'turn', status: 'complete', stopReason: 'end_turn' });
    expect((reducer.items()[0] as { entries: Array<{ type: string; entries?: unknown[] }> }).entries[0]).toMatchObject({
      type: 'plan',
      entries: [{ content: 'Run tests', status: 'completed' }],
    });
    expect(reducer.items()).toHaveLength(1);

    let seq = 4;
    for (const process of ['STARTING', 'RUNNING', 'STOPPED', 'DEAD']) {
      const result = reducer.ingest(event(seq++, 'state_change', { process, turn: 'IDLE' }));
      expect(result).toBeNull();
    }
    expect(reducer.ingest(event(seq++, 'state_change', { process: 'RUNNING', turn: 'CANCELLING' }))).toBeNull();
    expect(reducer.ingest(event(seq++, 'state_change', { process: 'RUNNING', turn: 'PROMPTING' }))).toBeNull();

    expect(reducer.items()).toHaveLength(1);
    expect(reducer.items().some((item) => (item as unknown as { type: string }).type === 'state_change')).toBe(false);
  });

  it('keeps process state events out of an empty transcript', () => {
    const reducer = new EventReducer();
    expect(reducer.ingest(event(1, 'state_change', { process: 'STARTING', turn: 'IDLE' }))).toBeNull();
    expect(reducer.ingest(event(2, 'state_change', { process: 'STOPPED', turn: 'IDLE' }))).toBeNull();
    expect(reducer.ingest(event(3, 'state_change', { process: 'DEAD', turn: 'IDLE' }))).toBeNull();
    expect(reducer.items()).toHaveLength(0);
  });

  it('rebuilds chronologically when older history arrives after live events', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(3, 'message_chunk', { text: 'reply' }));
    reducer.ingest(event(4, 'turn_complete', { stop_reason: 'end_turn' }));
    reducer.ingest(event(1, 'user_message', { text: 'question' }));
    reducer.ingest(event(2, 'message_chunk', { text: 'reply' }));
    reducer.ingest(event(3, 'message_chunk', { text: 'reply' }));

    expect(reducer.items()).toHaveLength(2);
    expect(reducer.items()[0]).toMatchObject({ type: 'user_message', text: 'question' });
    expect(reducer.items()[1]).toMatchObject({
      type: 'turn',
      status: 'complete',
      entries: [{ type: 'message_chunk', text: 'replyreply' }],
    });
  });

  it('reconstructs the active turn start from replayed state and user events', () => {
    const reducer = new EventReducer();
    reducer.ingest({
      ...event(4, 'message_chunk', { text: 'reply' }),
      timestamp: '2026-09-13T12:01:00Z',
    });
    reducer.ingest({
      ...event(5, 'state_change', { process: 'RUNNING', turn: 'PROMPTING' }),
      timestamp: '2026-09-13T12:00:05Z',
    });
    expect(reducer.turnStartedAt()).toBe('2026-09-13T12:00:05Z');

    reducer.ingest({
      ...event(1, 'user_message', { text: 'question' }),
      timestamp: '2026-09-13T12:00:00Z',
    });
    expect(reducer.turnStartedAt()).toBe('2026-09-13T12:00:00Z');

    reducer.ingest({
      ...event(6, 'turn_complete', { stop_reason: 'end_turn' }),
      timestamp: '2026-09-13T12:01:10Z',
    });
    expect(reducer.turnStartedAt()).toBeNull();
  });

  it('splits messages on message_id change and keeps old agents merging', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'message_chunk', { text: 'a ', message_id: 'm1' }));
    reducer.ingest(event(2, 'message_chunk', { text: 'b', message_id: 'm1' }));
    reducer.ingest(event(3, 'message_chunk', { text: 'c', message_id: 'm2' }));
    const entries = (reducer.items()[0] as { entries: Array<{ text: string }> }).entries;
    expect(entries).toHaveLength(2);
    expect(entries[0].text).toBe('a b');
    expect(entries[1].text).toBe('c');

    const legacy = new EventReducer();
    legacy.ingest(event(1, 'message_chunk', { text: 'x ' }));
    legacy.ingest(event(2, 'message_chunk', { text: 'y' }));
    expect((legacy.items()[0] as { entries: Array<{ text: string }> }).entries[0].text).toBe('x y');
  });

  it('preserves tool locations and resolves elicitation distinctly', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'tool_call', { id: 't1', title: 'Edit', status: 'in_progress', locations: [{ path: '/a/b.rs', line: 3 }] }));
    reducer.ingest(event(2, 'elicitation_request', { id: 'e1', mode: 'form', message: 'Need input' }));
    reducer.ingest(event(3, 'elicitation_response', { id: 'e1', action: 'accept' }));
    const entries = (
      reducer.items()[0] as {
        entries: Array<{ type: string; locations?: unknown; responded?: boolean; decision?: string }>;
      }
    ).entries;
    expect(entries[0]).toMatchObject({ type: 'tool_call', locations: [{ path: '/a/b.rs', line: 3 }] });
    expect(entries[1]).toMatchObject({ type: 'elicitation_request', responded: true, decision: 'Accepted' });
  });

  it('ignores session-level dynamic state for the transcript', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'available_commands', { commands: [] }));
    reducer.ingest(event(2, 'session_modes', { state: {} }));
    reducer.ingest(event(3, 'usage_update', { used: 1, size: 2 }));
    reducer.ingest(event(4, 'session_info', { title: 'Hi' }));
    expect(reducer.items()).toHaveLength(0);
  });

  it('merges split emphasis across streamed text content blocks', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'message_chunk', {
      text: '**bo', message_id: 'm1', content: [{ type: 'text', text: '**bo' }],
    }));
    reducer.ingest(event(2, 'message_chunk', {
      text: 'ld**', message_id: 'm1', content: [{ type: 'text', text: 'ld**' }],
    }));
    const entries = (reducer.items()[0] as { entries: Array<{ text: string; content?: Array<{ type: string; text?: string }> }> }).entries;
    expect(entries).toHaveLength(1);
    expect(entries[0].text).toBe('**bold**');
    expect(entries[0].content).toEqual([{ type: 'text', text: '**bold**' }]);
  });

  it('merges split inline code across streamed chunks', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'message_chunk', {
      text: '`co', message_id: 'm1', content: [{ type: 'text', text: '`co' }],
    }));
    reducer.ingest(event(2, 'message_chunk', {
      text: 'de`', message_id: 'm1', content: [{ type: 'text', text: 'de`' }],
    }));
    const entries = (reducer.items()[0] as { entries: Array<{ content?: Array<{ type: string; text?: string }> }> }).entries;
    expect(entries[0].content).toEqual([{ type: 'text', text: '`code`' }]);
  });

  it('merges a fenced code block split across chunks', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'message_chunk', {
      text: '```rust\nfn ', message_id: 'm1', content: [{ type: 'text', text: '```rust\nfn ' }],
    }));
    reducer.ingest(event(2, 'message_chunk', {
      text: 'main() {}\n```', message_id: 'm1', content: [{ type: 'text', text: 'main() {}\n```' }],
    }));
    const entries = (reducer.items()[0] as { entries: Array<{ content?: Array<{ type: string; text?: string }> }> }).entries;
    expect(entries[0].content).toEqual([{ type: 'text', text: '```rust\nfn main() {}\n```' }]);
  });

  it('merges a list split across chunks into one Markdown region', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'message_chunk', {
      text: '- item 1\n- it', message_id: 'm1', content: [{ type: 'text', text: '- item 1\n- it' }],
    }));
    reducer.ingest(event(2, 'message_chunk', {
      text: 'em 2\n', message_id: 'm1', content: [{ type: 'text', text: 'em 2\n' }],
    }));
    const entries = (reducer.items()[0] as { entries: Array<{ content?: Array<{ type: string; text?: string }> }> }).entries;
    expect(entries[0].content).toEqual([{ type: 'text', text: '- item 1\n- item 2\n' }]);
  });

  it('merges a heading split across chunks', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'message_chunk', {
      text: '## Sum', message_id: 'm1', content: [{ type: 'text', text: '## Sum' }],
    }));
    reducer.ingest(event(2, 'message_chunk', {
      text: 'mary\n', message_id: 'm1', content: [{ type: 'text', text: 'mary\n' }],
    }));
    const entries = (reducer.items()[0] as { entries: Array<{ content?: Array<{ type: string; text?: string }> }> }).entries;
    expect(entries[0].content).toEqual([{ type: 'text', text: '## Summary\n' }]);
  });

  it('keeps text/image/text as three regions and never merges across media', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'message_chunk', {
      text: 'A', message_id: 'm1', content: [{ type: 'text', text: 'A' }],
    }));
    reducer.ingest(event(2, 'message_chunk', {
      text: '', message_id: 'm1', content: [{ type: 'image', data: 'iVBORw0KGgo=', mimeType: 'image/png' }],
    }));
    reducer.ingest(event(3, 'message_chunk', {
      text: 'B', message_id: 'm1', content: [{ type: 'text', text: 'B' }],
    }));
    const entries = (reducer.items()[0] as { entries: Array<{ content?: unknown[] }> }).entries;
    expect(entries).toHaveLength(1);
    expect(entries[0].content).toEqual([
      { type: 'text', text: 'A' },
      { type: 'image', data: 'iVBORw0KGgo=', mimeType: 'image/png' },
      { type: 'text', text: 'B' },
    ]);
  });

  it('never merges text across audio, resource or resource_link blocks', () => {
    for (const barrier of [
      { type: 'audio', data: 'SUQz', mimeType: 'audio/mpeg' },
      { type: 'resource_link', name: 'safe', uri: 'https://example.test' },
      { type: 'resource', resource: { uri: 'attachment://note.txt', mimeType: 'text/plain', text: 'note' } },
    ]) {
      const reducer = new EventReducer();
      reducer.ingest(event(1, 'message_chunk', {
        text: 'before', message_id: 'm1', content: [{ type: 'text', text: 'before' }],
      }));
      reducer.ingest(event(2, 'message_chunk', { text: '', message_id: 'm1', content: [barrier] }));
      reducer.ingest(event(3, 'message_chunk', {
        text: 'after', message_id: 'm1', content: [{ type: 'text', text: 'after' }],
      }));
      const entries = (reducer.items()[0] as { entries: Array<{ content?: unknown[] }> }).entries;
      expect(entries[0].content).toEqual([
        { type: 'text', text: 'before' },
        barrier,
        { type: 'text', text: 'after' },
      ]);
    }
  });

  it('aggregates thought chunks with the same text merging', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'thought_chunk', {
      text: '**bo', message_id: 't1', content: [{ type: 'text', text: '**bo' }],
    }));
    reducer.ingest(event(2, 'thought_chunk', {
      text: 'ld**', message_id: 't1', content: [{ type: 'text', text: 'ld**' }],
    }));
    const entries = (reducer.items()[0] as { entries: Array<{ type: string; text: string; content?: Array<{ type: string; text?: string }> }> }).entries;
    expect(entries).toHaveLength(1);
    expect(entries[0].type).toBe('thought_chunk');
    expect(entries[0].content).toEqual([{ type: 'text', text: '**bold**' }]);
  });

  it('keeps thought text/image/text as three regions', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'thought_chunk', {
      text: 'A', message_id: 't1', content: [{ type: 'text', text: 'A' }],
    }));
    reducer.ingest(event(2, 'thought_chunk', {
      text: '', message_id: 't1', content: [{ type: 'image', data: 'iVBORw0KGgo=', mimeType: 'image/png' }],
    }));
    reducer.ingest(event(3, 'thought_chunk', {
      text: 'B', message_id: 't1', content: [{ type: 'text', text: 'B' }],
    }));
    const entries = (reducer.items()[0] as { entries: Array<{ content?: unknown[] }> }).entries;
    expect(entries[0].content).toEqual([
      { type: 'text', text: 'A' },
      { type: 'image', data: 'iVBORw0KGgo=', mimeType: 'image/png' },
      { type: 'text', text: 'B' },
    ]);
  });

  it('does not merge text content across different message ids', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'message_chunk', {
      text: '**bo', message_id: 'm1', content: [{ type: 'text', text: '**bo' }],
    }));
    reducer.ingest(event(2, 'message_chunk', {
      text: 'ld**', message_id: 'm2', content: [{ type: 'text', text: 'ld**' }],
    }));
    const entries = (reducer.items()[0] as { entries: Array<{ content?: unknown[] }> }).entries;
    expect(entries).toHaveLength(2);
    expect(entries[0].content).toEqual([{ type: 'text', text: '**bo' }]);
    expect(entries[1].content).toEqual([{ type: 'text', text: 'ld**' }]);
  });

  it('keeps legacy adjacency merging for agents that omit message ids', () => {
    const reducer = new EventReducer();
    reducer.ingest(event(1, 'message_chunk', {
      text: '**bo', content: [{ type: 'text', text: '**bo' }],
    }));
    reducer.ingest(event(2, 'message_chunk', {
      text: 'ld**', content: [{ type: 'text', text: 'ld**' }],
    }));
    const entries = (reducer.items()[0] as { entries: Array<{ content?: unknown[] }> }).entries;
    expect(entries).toHaveLength(1);
    expect(entries[0].content).toEqual([{ type: 'text', text: '**bold**' }]);
  });
});
