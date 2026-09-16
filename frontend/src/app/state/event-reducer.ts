import { signal } from '@angular/core';
import type {
  DisplayError,
  DisplayItem,
  DisplayTurn,
  DisplayUserMessage,
  RichContentBlock,
  SessionEvent,
  TurnEntry,
  TurnEntryTool,
} from '../core/api/types';

/**
 * Aggregates session events into the display list.
 *
 * Every update replaces the item, the turn and the entry array instead of
 * mutating them. The application runs zoneless, so a consumer only re-renders
 * when the identity of the value it reads changes.
 */
export class EventReducer {
  private nextId = 1;
  private readonly eventsBySeq = new Map<number, SessionEvent>();
  private readonly itemList = signal<DisplayItem[]>([]);
  readonly items = this.itemList.asReadonly();
  private readonly turnStart = signal<string | null>(null);
  readonly turnStartedAt = this.turnStart.asReadonly();
  private turnStartSource: 'user' | 'state' | 'inferred' | null = null;
  private currentTurnId: number | null = null;

  constructor(initialEvents: SessionEvent[] = []) {
    for (const event of initialEvents) this.eventsBySeq.set(event.seq, event);
    this.rebuild();
  }

  ingest(event: SessionEvent): DisplayItem | null {
    if (typeof event.seq !== 'number' || this.eventsBySeq.has(event.seq)) return null;
    this.eventsBySeq.set(event.seq, event);
    this.rebuild();
    if (!event.payload || event.payload.type === 'state_change') return null;
    return this.itemList().at(-1) ?? null;
  }

  /**
   * Pages can arrive older than live events. Replaying the durable sequence
   * from the merged map keeps turn grouping and permission resolution
   * chronological regardless of arrival order.
   */
  private rebuild(): void {
    this.nextId = 1;
    this.currentTurnId = null;
    this.turnStart.set(null);
    this.turnStartSource = null;
    this.itemList.set([]);
    const ordered = [...this.eventsBySeq.values()].sort((a, b) => a.seq - b.seq);
    for (const event of ordered) this.ingestOrdered(event);
  }

  private ingestOrdered(event: SessionEvent): DisplayItem | null {
    const payload = event.payload;
    if (!payload) return null;

    if (payload.type === 'user_message') {
      this.turnStart.set(event.timestamp);
      this.turnStartSource = 'user';
    } else if (
      payload.type === 'state_change'
      && payload.turn === 'PROMPTING'
      && this.turnStartSource !== 'user'
    ) {
      this.turnStart.set(event.timestamp);
      this.turnStartSource = 'state';
    } else if (payload.type === 'turn_complete') {
      this.turnStart.set(null);
      this.turnStartSource = null;
    } else if (this.isTurnScoped(payload.type) && !this.turnStart()) {
      this.turnStart.set(event.timestamp);
      this.turnStartSource = 'inferred';
    }

    if (this.isTurnScoped(payload.type)) {
      return this.ingestTurnEvent(event);
    }

    if (payload.type === 'user_message') {
      this.closeCurrentTurn();
      const messageId = this.stringValue(payload['message_id']);
      return this.append<DisplayUserMessage>({
        id: this.nextId++,
        type: 'user_message',
        text: this.stringValue(payload.text) ?? '',
        ...(this.contentBlocks(payload.content) ? { content: this.contentBlocks(payload.content)! } : {}),
        timestamp: event.timestamp,
        ...(messageId != null ? { messageId } : {}),
      });
    }

    if (payload.type === 'error') {
      return this.append<DisplayError>({
        id: this.nextId++,
        type: 'error',
        message: this.stringValue(payload.message) ?? 'Unknown error',
        timestamp: event.timestamp,
      });
    }

    if (payload.type === 'state_change') {
      return null;
    }

    return null;
  }

  private append<T extends DisplayItem>(item: T): T {
    this.itemList.update((items) => [...items, item]);
    return item;
  }

  /** Marks an open turn complete without emitting a stop reason. */
  private closeCurrentTurn(): void {
    const open = this.openTurn();
    if (!open) return;
    const closed: DisplayTurn = { ...open, status: 'complete' };
    this.itemList.update((items) => items.map((item) => (item.id === closed.id ? closed : item)));
    this.currentTurnId = null;
  }

  private openTurn(): DisplayTurn | null {
    if (this.currentTurnId === null) return null;
    const found = this.itemList().find((item) => item.id === this.currentTurnId);
    return found && found.type === 'turn' ? found : null;
  }

  private ingestTurnEvent(event: SessionEvent): DisplayItem {
    const payload = event.payload;
    const open = this.openTurn();
    const isNew = open === null;
    const base: DisplayTurn = open ?? {
      id: this.nextId++,
      type: 'turn',
      agent: event.agent || 'Agent',
      timestamp: event.timestamp,
      completedAt: null,
      status: 'in_progress',
      stopReason: null,
      entries: [],
    };

    let turn = this.applyTurnEvent(base, event);
    const handledHere = turn !== base;

    if (payload.type === 'turn_complete') {
      turn = {
        ...turn,
        status: 'complete',
        completedAt: event.timestamp,
        stopReason: this.stringValue(payload.stop_reason ?? payload.stopReason) ?? null,
      };
    }

    const next = turn;
    this.itemList.update((items) => {
      const merged = isNew
        ? [...items, next]
        : items.map((item) => (item.id === next.id ? next : item));

      // A permission/elicitation response can resolve a request raised in an
      // earlier turn.
      if (payload.type === 'permission_response' && !handledHere) {
        return this.markPermissionInItems(merged, next.id, payload);
      }
      if (
        (payload.type === 'elicitation_response' ||
          payload.type === 'elicitation_complete') &&
        !handledHere
      ) {
        return this.markElicitationInItems(merged, next.id, payload);
      }
      return merged;
    });

    this.currentTurnId = payload.type === 'turn_complete' ? null : next.id;
    return next;
  }

  private isTurnScoped(type: string): boolean {
    return [
      'message_chunk',
      'thought_chunk',
      'tool_call',
      'tool_call_update',
      'plan',
      'permission_request',
      'permission_response',
      'elicitation_request',
      'elicitation_response',
      'elicitation_complete',
      'turn_complete',
    ].includes(type);
  }

  /** Returns a new turn when the event applies to it, otherwise the same turn. */
  private applyTurnEvent(turn: DisplayTurn, event: SessionEvent): DisplayTurn {
    const payload = event.payload;
    const entries = turn.entries;
    const last = entries[entries.length - 1];

    if (payload.type === 'message_chunk' || payload.type === 'thought_chunk') {
      const text = this.stringValue(payload.text) ?? '';
      const content = this.contentBlocks(payload.content);
      const messageId = this.stringValue((payload as Record<string, unknown>)['message_id']);
      // Same messageId means one message; a change starts a new entry.
      // Old agents omit IDs and merge by adjacency.
      if (
        last &&
        last.type === payload.type &&
        (messageId == null ||
          (last as { messageId?: string }).messageId === messageId)
      ) {
        // Preserve the ID on the merged entry for later chunks.
        // Adjacent text blocks are streaming deltas of one Markdown region,
        // so concatenate them. Non-text blocks always break the run.
        const previousContent = (last as { content?: RichContentBlock[] }).content;
        const mergedContent = this.mergeContentBlocks(previousContent, content);
        const merged: TurnEntry = {
          ...last,
          text: (last as { text: string }).text + text,
          ...(mergedContent ? { content: mergedContent } : {}),
          ...(messageId != null ? { messageId } : {}),
        } as TurnEntry;
        return { ...turn, entries: [...entries.slice(0, -1), merged] };
      }
      return {
        ...turn,
        entries: [
          ...entries,
          {
            id: this.nextId++,
            type: payload.type,
            text,
            ...(content ? { content } : {}),
            ...(messageId != null ? { messageId } : {}),
          } as TurnEntry,
        ],
      };
    }

    if (payload.type === 'tool_call') {
      const toolId =
        this.stringValue(payload.id ?? payload.toolCallId ?? payload.tool_call_id) ??
        String(this.nextId);
      const kind = this.stringValue(payload['kind']);
      const parentId = this.stringValue(payload['parent_id'] ?? payload['parentId']);
      const locations = Array.isArray(payload['locations'])
        ? (payload['locations'] as Array<{ path: string; line?: number | null }>)
        : null;
      return {
        ...turn,
        entries: [
          ...entries,
          {
            id: this.nextId++,
            type: 'tool_call',
            toolCallId: toolId,
            title: this.stringValue(payload.title) ?? 'Tool Call',
            status: this.stringValue(payload.status) ?? 'in_progress',
            output: null,
            kind,
            parentId,
            locations,
            ...(payload.content !== undefined ? { content: payload.content } : {}),
          },
        ],
      };
    }

    if (payload.type === 'tool_call_update') {
      const toolId =
        this.stringValue(payload.id ?? payload.toolCallId ?? payload.tool_call_id) ?? '';
      const locations = Array.isArray(payload['locations'])
        ? (payload['locations'] as Array<{ path: string; line?: number | null }>)
        : undefined;
      const index = this.findToolCallIndex(entries, toolId);
      if (index >= 0) {
        const tool = entries[index] as TurnEntryTool;
        const status = this.stringValue(payload.status);
        const title = this.stringValue(payload.title);
        const kind = this.stringValue(payload['kind']);
        const updated: TurnEntryTool = {
          ...tool,
          title: title ?? tool.title,
          kind: kind ?? tool.kind,
          status: status ?? tool.status,
          output:
            payload.output !== undefined && payload.output !== null
              ? (tool.output || '') + String(payload.output)
              : tool.output,
          ...(locations !== undefined ? { locations } : {}),
          ...(payload.content !== undefined ? { content: payload.content } : {}),
        };
        const nextEntries = [...entries];
        nextEntries[index] = updated;
        return { ...turn, entries: nextEntries };
      }
      return {
        ...turn,
        entries: [
          ...entries,
          {
            id: this.nextId++,
            type: 'tool_call',
            toolCallId: toolId,
            title: this.stringValue(payload.title) ?? 'Tool Call',
            status: this.stringValue(payload.status) ?? 'in_progress',
            output: payload.output == null ? null : String(payload.output),
            kind: this.stringValue(payload['kind']),
            parentId: this.stringValue(payload['parent_id'] ?? payload['parentId']),
            locations: locations ?? null,
            ...(payload.content !== undefined ? { content: payload.content } : {}),
          },
        ],
      };
    }

    if (payload.type === 'plan') {
      const entryList = Array.isArray(payload.entries) ? payload.entries : [];
      const planEntries = entryList as Array<{ content: string; status: string }>;
      if (last?.type === 'plan') {
        const merged: TurnEntry = { ...last, entries: planEntries };
        return { ...turn, entries: [...entries.slice(0, -1), merged] };
      }
      return {
        ...turn,
        entries: [...entries, { id: this.nextId++, type: 'plan', entries: planEntries }],
      };
    }

    if (payload.type === 'permission_request') {
      return {
        ...turn,
        entries: [
          ...entries,
          {
            id: this.nextId++,
            type: 'permission_request',
            requestId: this.stringValue(payload.id) ?? '',
            method: this.stringValue(payload.method) ?? '',
            description: this.stringValue(payload.description) ?? '',
            title: this.stringValue(payload['title']),
            kind: this.stringValue(payload['kind']),
            options: this.permissionOptions(payload.options),
            responded: false,
          },
        ],
      };
    }


    if (payload.type === 'permission_response') {
      const marked = this.markPermission(entries, payload);
      return marked ? { ...turn, entries: marked } : turn;
    }

    if (payload.type === 'elicitation_request') {
      return {
        ...turn,
        entries: [
          ...entries,
          {
            id: this.nextId++,
            type: 'elicitation_request',
            requestId: this.stringValue(payload.id) ?? '',
            mode: this.stringValue(payload['mode']) ?? 'form',
            message: this.stringValue(payload['message']) ?? '',
            schema: payload['schema'],
            url: this.stringValue(payload['url']),
            toolCallId: this.stringValue(payload['tool_call_id']),
            responded: false,
          },
        ],
      };
    }

    if (payload.type === 'elicitation_response') {
      const marked = this.markElicitation(entries, payload);
      return marked ? { ...turn, entries: marked } : turn;
    }

    if (payload.type === 'elicitation_complete') {
      const marked = this.markElicitationComplete(entries, payload);
      return marked ? { ...turn, entries: marked } : turn;
    }

    return turn;
  }

  /** Returns new entries with the first matching permission resolved, else null. */
  private markPermission(
    entries: readonly TurnEntry[],
    payload: SessionEvent['payload'],
  ): TurnEntry[] | null {
    const permissionId = this.stringValue(payload.id);
    const index = entries.findIndex(
      (entry) =>
        entry.type === 'permission_request' &&
        (!permissionId || entry.requestId === permissionId),
    );
    if (index < 0) return null;
    const next = [...entries];
    next[index] = {
      ...next[index],
      responded: true,
      decision: this.permissionDecision(payload),
    } as TurnEntry;
    return next;
  }

  private permissionDecision(payload: SessionEvent['payload']): string {
    const optionId = this.stringValue(payload['option_id']);
    if (optionId) return optionId;
    if (typeof payload['granted'] === 'boolean') {
      return payload['granted'] ? 'Allowed' : 'Denied';
    }
    return 'Cancelled';
  }

  private permissionOptions(value: unknown): import('../core/api/types').AgentPermissionOption[] {
    if (!Array.isArray(value)) return [];
    return value.filter((option): option is import('../core/api/types').AgentPermissionOption =>
      typeof option === 'object' && option !== null
      && typeof (option as Record<string, unknown>)['optionId'] === 'string'
      && typeof (option as Record<string, unknown>)['name'] === 'string'
      && typeof (option as Record<string, unknown>)['kind'] === 'string',
    );
  }

  private markPermissionInItems(
    items: DisplayItem[],
    skipTurnId: number,
    payload: SessionEvent['payload'],
  ): DisplayItem[] {
    for (let index = 0; index < items.length; index += 1) {
      const item = items[index];
      if (item.type !== 'turn' || item.id === skipTurnId) continue;
      const marked = this.markPermission(item.entries, payload);
      if (!marked) continue;
      const next = [...items];
      next[index] = { ...item, entries: marked };
      return next;
    }
    return items;
  }

  private markElicitation(
    entries: readonly TurnEntry[],
    payload: SessionEvent['payload'],
  ): TurnEntry[] | null {
    const id = this.stringValue(payload.id);
    const action = this.stringValue(payload['action']) ?? 'cancel';
    const index = entries.findIndex(
      (entry) =>
        entry.type === 'elicitation_request' && (!id || entry.requestId === id),
    );
    if (index < 0) return null;
    const next = [...entries];
    const label = action === 'accept' ? 'Accepted' : action === 'decline' ? 'Declined' : 'Cancelled';
    next[index] = { ...next[index], responded: true, decision: label } as TurnEntry;
    return next;
  }

  private markElicitationComplete(
    entries: readonly TurnEntry[],
    payload: SessionEvent['payload'],
  ): TurnEntry[] | null {
    const eid = this.stringValue(payload['elicitation_id'] ?? payload.id);
    const index = entries.findIndex(
      (entry) =>
        entry.type === 'elicitation_request' &&
        (!eid || entry.requestId === eid || (entry as { toolCallId?: string }).toolCallId === eid),
    );
    if (index < 0) return null;
    const next = [...entries];
    next[index] = { ...next[index], responded: true, decision: 'Completed' } as TurnEntry;
    return next;
  }

  private markElicitationInItems(
    items: DisplayItem[],
    skipTurnId: number,
    payload: SessionEvent['payload'],
  ): DisplayItem[] {
    for (let index = 0; index < items.length; index += 1) {
      const item = items[index];
      if (item.type !== 'turn' || item.id === skipTurnId) continue;
      const marked =
        payload.type === 'elicitation_complete'
          ? this.markElicitationComplete(item.entries, payload)
          : this.markElicitation(item.entries, payload);
      if (!marked) continue;
      const next = [...items];
      next[index] = { ...item, entries: marked };
      return next;
    }
    return items;
  }

  private findToolCallIndex(entries: readonly TurnEntry[], id: string): number {
    for (let index = entries.length - 1; index >= 0; index -= 1) {
      const entry = entries[index];
      if (entry.type === 'tool_call' && entry.toolCallId === id) return index;
    }
    return -1;
  }

  private contentBlocks(value: unknown): RichContentBlock[] | undefined {
    if (!Array.isArray(value)) return undefined;
    return value.filter((block): block is RichContentBlock =>
      !!block && typeof block === 'object' && typeof (block as { type?: unknown }).type === 'string',
    );
  }

  /**
   * Merges streamed rich-content deltas into display regions.
   *
   * Real agents send one text fragment per `agent_message_chunk` /
   * `agent_thought_chunk`. Concatenating adjacent text blocks keeps one
   * coherent Markdown string per region, so `**bo` + `ld**` renders as bold
   * instead of two malformed fragments. An image, audio block, embedded
   * resource or resource link always breaks the run: text A, image, text B
   * stays three regions. Only the boundary blocks can merge; the rest keep
   * their order.
   */
  private mergeContentBlocks(
    previous: RichContentBlock[] | undefined,
    incoming: RichContentBlock[] | undefined,
  ): RichContentBlock[] | undefined {
    if (!incoming || incoming.length === 0) return previous;
    if (!previous || previous.length === 0) return [...incoming];
    const prevLast = previous[previous.length - 1];
    const nextFirst = incoming[0];
    if (prevLast.type === 'text' && nextFirst.type === 'text') {
      const mergedText: RichContentBlock = {
        ...prevLast,
        text: prevLast.text + nextFirst.text,
      };
      return [...previous.slice(0, -1), mergedText, ...incoming.slice(1)];
    }
    return [...previous, ...incoming];
  }

  private stringValue(value: unknown): string | undefined {
    return typeof value === 'string' ? value : value == null ? undefined : String(value);
  }
}
