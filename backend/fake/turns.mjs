// Scripted agent turns. A turn streams the same event sequence that a real
// ACP agent produces, so the frontend reducer and the chat UI get exercised
// without a compiled backend and without an agent binary.

import { randomUUID } from 'node:crypto';

import { PROJECT_ROOT, RICH_HISTORY_CONTENT } from './state.mjs';

/** Turns that are running now, by chat id. */
const active = new Map();

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * Chooses the scenario from the text of the prompt.
 * The keyword makes a demo reproducible.
 */
export function scenarioFor(text) {
  const lower = text.toLowerCase();
  if (lower.includes('error')) return 'error';
  if (lower.includes('plan-approve')) return 'plan-approve';
  if (lower.includes('permission')) return 'permission';
  if (lower.includes('elicit-url')) return 'elicit-url';
  if (lower.includes('elicit')) return 'elicit-form';
  if (lower.includes('commands')) return 'commands';
  if (lower.includes('modes')) return 'modes';
  if (lower.includes('usage')) return 'usage';
  if (lower.includes('message-id')) return 'message-id';
  if (lower.includes('streaming-markdown') || lower.includes('markdown') || lower.includes('stream')) return 'streaming-markdown';
  if (lower.includes('plan')) return 'plan';
  if (lower.includes('rich')) return 'rich';
  if (lower.includes('terminal') || lower.includes('task')) return 'terminal';
  if (lower.includes('tool')) return 'tools';
  if (lower.includes('long')) return 'long';
  if (lower.includes('quiet')) return 'quiet';
  return 'full';
}

export function isRunning(chatId) {
  return active.has(chatId);
}

/** Marks the turn for cancellation. Cancellation resolves pending
 *  permission/elicitation as cancelled, never as denial. */
export function cancel(chatId) {
  const turn = active.get(chatId);
  if (!turn) return false;
  turn.cancelled = true;
  // None/feigned cancel: resolve with null so the turn records `cancelled`,
  // not a user denial.
  turn.resolvePermission?.(null);
  turn.resolveElicitation?.({ action: 'cancel' });
  turn.resolveHold?.();
  return true;
}

/** Resolves a permission request with the exact agent option id. */
export function answerPermission(chatId, requestId, optionId) {
  const turn = active.get(chatId);
  if (!turn || turn.permissionId !== requestId) return false;
  if (!turn.permissionOptionIds?.has(optionId)) return false;
  turn.resolvePermission?.(optionId);
  return true;
}

const pendingElicitations = new Map();

export function listPendingElicitations(chatId) {
  const byChat = pendingElicitations.get(chatId);
  return byChat ? [...byChat.values()].map(({ info }) => info) : [];
}

export function answerElicitation(chatId, id, action, content) {
  const byChat = pendingElicitations.get(chatId);
  const pending = byChat?.get(id);
  if (!pending) return false;
  byChat.delete(id);
  pending.resolve({ action, content });
  return true;
}

export function cancelElicitations(chatId) {
  const byChat = pendingElicitations.get(chatId);
  if (!byChat) return;
  for (const [id, pending] of byChat) {
    pending.resolve({ action: 'cancel' });
  }
  byChat.clear();
}

/**
 * Runs one turn. It returns at once; the turn streams in the background,
 * exactly like the `tokio::spawn` of `hub::prompt`.
 */
export function startTurn(state, chat, text, latency, content = undefined) {
  const turn = {
    cancelled: false,
    permissionId: null,
    resolvePermission: null,
    permissionOptionIds: new Set(['seed-allow-once', 'seed-reject-once']),
    resolveHold: null,
  };
  active.set(chat.id, turn);

  runTurn(state, chat, text, latency, turn, content)
    .catch((error) => {
      state.emit(chat.id, chat.agent, { type: 'error', message: String(error) });
    })
    .finally(() => {
      active.delete(chat.id);
      state.setRuntime(chat.id, 'RUNNING', 'IDLE');
    });
}

/**
 * Rehydrates an intentionally open fixture turn. The state layer has already
 * persisted the user message, partial output, and (for waiting) permission
 * request; this only supplies the live resolver that makes the normal
 * permission/cancel endpoints behave like a running ACP session.
 */
export function seedActiveTurn(state, chat, seed) {
  if (active.has(chat.id)) return active.get(chat.id);

  const turn = {
    cancelled: false,
    permissionId: seed.kind === 'waiting' ? seed.permission_id : null,
    permissionOptionIds: new Set(['seed-allow-once', 'seed-reject-once']),
    resolvePermission: null,
    resolveElicitation: null,
    resolveHold: null,
  };
  active.set(chat.id, turn);

  if (seed.kind === 'waiting') {
    const answered = new Promise((resolve) => {
      turn.resolvePermission = resolve;
    });
    void answered
      .then((granted) => finishSeededPermission(state, chat, turn, granted))
      .catch((error) => {
        state.emit(chat.id, chat.agent, { type: 'error', message: String(error) });
        state.emit(chat.id, chat.agent, { type: 'turn_complete', stop_reason: 'error' });
      })
      .finally(() => finishSeededTurn(state, chat, turn));
  } else {
    const held = new Promise((resolve) => {
      turn.resolveHold = resolve;
    });
    void held.finally(() => {
      finishCancelled((payload) => state.emit(chat.id, chat.agent, payload));
      finishSeededTurn(state, chat, turn);
    });
  }

  return turn;
}

async function finishSeededPermission(state, chat, turn, optionId) {
  const emit = (payload) => state.emit(chat.id, chat.agent, payload);
  const requestId = turn.permissionId;
  turn.permissionId = null;
  turn.resolvePermission = null;

  if (optionId === null || turn.cancelled) {
    finishCancelled(emit);
    return;
  }

  emit({ type: 'permission_response', id: requestId, option_id: optionId });
  await sleep(150);
  if (turn.cancelled) {
    finishCancelled(emit);
    return;
  }
  if (optionId === 'seed-reject-once') {
    emit({ type: 'message_chunk', text: 'I stopped because the WebSocket edit was denied.' });
    emit({ type: 'turn_complete', stop_reason: 'refusal' });
    return;
  }

  const toolId = 'seed-waiting-tool';
  emit({ type: 'tool_call_update', id: toolId, status: 'completed', output: '+12 -4' });
  emit({
    type: 'message_chunk',
    text: 'Permission granted. I preserved the sender high-water mark and the reconnect test now passes.',
  });
  emit({ type: 'turn_complete', stop_reason: 'end_turn' });
}

function finishSeededTurn(state, chat, turn) {
  if (active.get(chat.id) !== turn) return;
  active.delete(chat.id);
  state.setRuntime(chat.id, 'RUNNING', 'IDLE');
}

async function runTurn(state, chat, text, latency, turn, richContent = undefined) {
  const agent = chat.agent;
  const emit = (payload) => state.emit(chat.id, agent, payload);
  const pause = (ms) => sleep(Math.round(ms * latency));

  /** Streams text in chunks, so the UI shows a growing message. */
  const stream = async (type, full) => {
    for (const word of full.split(' ')) {
      if (turn.cancelled) return;
      emit({ type, text: `${word} ` });
      await pause(35);
    }
  };

  const userEvent = emit({ type: 'user_message', text, ...(richContent ? { content: richContent } : {}) });
  state.touchChatActivity(chat.id, userEvent.timestamp);
  state.setRuntime(chat.id, 'RUNNING', 'PROMPTING');
  await pause(200);

  const scenario = scenarioFor(text);

  if (scenario === 'error') {
    await stream('thought_chunk', 'Let me open the file the prompt names.');
    await pause(200);
    emit({
      type: 'error',
      message: 'Agent process exited with code 1: failed to spawn `claude-agent-acp`',
    });
    emit({ type: 'turn_complete', stop_reason: 'error' });
    return;
  }

  if (scenario === 'quiet') {
    await stream('message_chunk', 'Done.');
    emit({ type: 'turn_complete', stop_reason: 'end_turn' });
    return;
  }

  if (scenario === 'rich') {
    emit({
      type: 'thought_chunk',
      text: 'I will compare the attached screenshot with the retained render trace metadata.',
    });
    await pause(150);
    const content = RICH_HISTORY_CONTENT.map((block) => ({ ...block }));
    const toolId = randomUUID();
    emit({
      type: 'tool_call',
      id: toolId,
      title: 'Inspect the attached render trace',
      kind: 'read',
      status: 'in_progress',
      content: [{ type: 'content', content: content[0] }],
    });
    await pause(250);
    emit({
      type: 'tool_call_update',
      id: toolId,
      status: 'completed',
      output: 'firstPaintMs=184; layoutShift=0.01',
      content: [{ type: 'content', content: content[3] }],
    });
    emit({
      type: 'message_chunk',
      text: 'The screenshot matches the route. The small shift is caused by the deferred status badge.',
      content: content.slice(1),
    });
    emit({ type: 'turn_complete', stop_reason: 'end_turn' });
    return;
  }

  if (scenario === 'terminal') {
    const task = state.createTask(
      chat.id,
      'npm test -- --watch=false',
      `${PROJECT_ROOT}/batey/frontend`,
      'starting frontend tests…',
    );
    const toolId = randomUUID();
    emit({
      type: 'tool_call',
      id: toolId,
      title: 'Run frontend test suite',
      kind: 'execute',
      status: 'in_progress',
    });
    await pause(350);
    if (turn.cancelled) return finishCancelled(emit);
    emit({
      type: 'tool_call_update',
      id: toolId,
      status: 'in_progress',
      output: `task ${task.id.slice(0, 8)} is still running; follow it in Terminal tasks`,
    });
    emit({
      type: 'message_chunk',
      text: 'The frontend test suite is running in the background. The task remains available from the chat header.',
    });
    emit({ type: 'turn_complete', stop_reason: 'end_turn' });
    return;
  }

  if (scenario === 'plan-approve') {
    const steps = [
      { content: 'Inspect existing codebase architecture', status: 'completed' },
      { content: 'Implement plan approval dialog & markdown rendering', status: 'in_progress' },
      { content: 'Verify with integration tests', status: 'pending' },
    ];
    emit({ type: 'plan', entries: steps });
    await pause(300);

    const requestId = randomUUID();
    turn.permissionId = requestId;
    turn.permissionOptionIds = new Set(['plan-allow-once', 'plan-reject-once']);
    const answered = new Promise((resolve) => {
      turn.resolvePermission = resolve;
    });

    state.emit(chat.id, chat.agent, {
      type: 'permission_request',
      id: requestId,
      method: 'session/request_permission',
      title: 'Approve Plan',
      kind: 'switch_mode',
      description: '### Proposed Implementation Plan\n\n1. **Inspect Codebase**: Check backend ACP handlers and session timeouts.\n2. **Frontend Updates**: Render plans with rich markdown and provide dedicated approve/reject actions.\n3. **Validation**: Run end-to-end and unit test suites.',
      options: [
        { optionId: 'plan-allow-once', name: 'Approve once', kind: 'allow_once' },
        { optionId: 'plan-reject-once', name: 'Reject once', kind: 'reject_once' },
      ],
    });

    const optionId = await answered;
    turn.permissionId = null;
    turn.resolvePermission = null;

    // Cancellation is not a denial: finish as cancelled without recording
    // a denial response.
    if (optionId === null || turn.cancelled) return finishCancelled(emit);

    state.emit(chat.id, chat.agent, {
      type: 'permission_response',
      id: requestId,
      option_id: optionId,
    });
    await pause(200);

    if (optionId === 'plan-reject-once') {
      await stream('message_chunk', 'Plan was rejected. Please provide feedback on what to change.');
      emit({ type: 'turn_complete', stop_reason: 'refusal' });
      return;
    }
    await stream('message_chunk', 'Plan approved! Starting implementation now.');
    emit({ type: 'turn_complete', stop_reason: 'end_turn' });
    return;
  }

  if (scenario === 'commands') {
    const commands = [
      { name: 'plan', description: 'Create an implementation plan', input: { hint: 'goal for the plan' } },
      { name: 'review', description: 'Review the current changes' },
    ];
    state.commandsByChat.set(chat.id, commands);
    emit({ type: 'available_commands', commands });
    await pause(200);
    await stream('message_chunk', 'Commands are ready. Type / to discover them.');
    emit({ type: 'turn_complete', stop_reason: 'end_turn' });
    return;
  }

  if (scenario === 'modes') {
    const modes = {
      current_mode_id: 'act',
      available_modes: [
        { id: 'ask', name: 'Ask', description: 'Ask before acting' },
        { id: 'act', name: 'Act', description: 'Act without asking' },
      ],
    };
    state.modesByChat.set(chat.id, modes);
    emit({ type: 'session_modes', state: modes });
    await pause(200);
    await stream('message_chunk', 'Switched to Act mode.');
    emit({ type: 'turn_complete', stop_reason: 'end_turn' });
    return;
  }

  if (scenario === 'usage') {
    const usage = { used: 4500, size: 200000, cost_amount: 0.045, cost_currency: 'USD' };
    state.usageByChat.set(chat.id, usage);
    emit({ type: 'usage_update', ...usage });
    await pause(200);
    await stream('message_chunk', 'Usage updated for this session.');
    emit({ type: 'turn_complete', stop_reason: 'end_turn' });
    return;
  }

  if (scenario === 'message-id') {
    // Same messageId means one message; a change starts a new one.
    // Old clients that omit IDs still merge by adjacency.
    emit({ type: 'message_chunk', text: 'First part ', message_id: 'msg-1' });
    await pause(100);
    emit({ type: 'message_chunk', text: 'second part.', message_id: 'msg-1' });
    await pause(100);
    emit({ type: 'message_chunk', text: 'A new message.', message_id: 'msg-2' });
    await pause(100);
    emit({ type: 'turn_complete', stop_reason: 'end_turn' });
    return;
  }

  if (scenario === 'streaming-markdown') {
    // Streams the same Markdown a real agent sends as token fragments, so
    // the display layer must merge adjacent text blocks into one coherent
    // document instead of parsing each fragment alone. Comparable to Codex,
    // Claude and OpenCode chunking.
    const thoughtId = 'thought-stream-1';
    for (const fragment of ['The plan is **bo', 'ld** and `co', 'de`.']) {
      if (turn.cancelled) return finishCancelled(emit);
      emit({ type: 'thought_chunk', text: fragment, message_id: thoughtId, content: [{ type: 'text', text: fragment }] });
      await pause(60);
    }
    const messageId = 'msg-stream-1';
    const fragments = [
      '## Summary\n',
      'This fixes **bo',
      'ld** and `co',
      'de`.\n',
      '```rust\nfn ',
      'main() {}\n```\n',
      '- item 1\n- it',
      'em 2\n',
    ];
    for (const fragment of fragments) {
      if (turn.cancelled) return finishCancelled(emit);
      emit({ type: 'message_chunk', text: fragment, message_id: messageId, content: [{ type: 'text', text: fragment }] });
      await pause(60);
    }
    if (turn.cancelled) return finishCancelled(emit);
    // Text, image, text stays three regions; the image breaks text merging.
    const mixedId = 'msg-stream-2';
    emit({ type: 'message_chunk', text: 'Before screenshot ', message_id: mixedId, content: [{ type: 'text', text: 'Before screenshot ' }] });
    await pause(60);
    emit({
      type: 'message_chunk', text: '', message_id: mixedId,
      content: [{ type: 'image', data: RICH_HISTORY_CONTENT[1].data, mimeType: 'image/png' }],
    });
    await pause(60);
    emit({ type: 'message_chunk', text: ' after screenshot.', message_id: mixedId, content: [{ type: 'text', text: ' after screenshot.' }] });
    await pause(60);
    emit({ type: 'turn_complete', stop_reason: 'end_turn' });
    return;
  }

  if (scenario === 'elicit-form' || scenario === 'elicit-url') {
    const isUrl = scenario === 'elicit-url';
    const eid = randomUUID();
    const info = isUrl
      ? {
          id: eid,
          mode: 'url',
          message: 'Sign in to continue',
          url: 'https://example.invalid/auth',
          elicitation_id: eid,
          tool_call_id: null,
        }
      : {
          id: eid,
          mode: 'form',
          message: 'Provide details to continue',
          schema: {
            type: 'object',
            properties: {
              name: { type: 'string', title: 'Name' },
              age: { type: 'integer', title: 'Age' },
            },
            required: ['name'],
          },
          elicitation_id: null,
          tool_call_id: null,
        };
    if (!pendingElicitations.has(chat.id)) pendingElicitations.set(chat.id, new Map());
    const byChat = pendingElicitations.get(chat.id);
    const answered = new Promise((resolve) => {
      byChat.set(eid, { info, resolve });
      turn.resolveElicitation = (result) => resolve(result);
    });
    const list = state.elicitationsByChat.get(chat.id) ?? [];
    list.push(info);
    state.elicitationsByChat.set(chat.id, list);
    emit({ type: 'elicitation_request', ...info });
    const result = await answered;
    turn.resolveElicitation = null;
    byChat.delete(eid);
    state.elicitationsByChat.set(
      chat.id,
      (state.elicitationsByChat.get(chat.id) ?? []).filter((e) => e.id !== eid),
    );
    const action = result?.action ?? 'cancel';
    // Form values never persist; only the action marker is recorded.
    emit({ type: 'elicitation_response', id: eid, action });
    if (isUrl && action === 'accept') {
      emit({ type: 'elicitation_complete', elicitation_id: eid });
    }
    if (action === 'cancel' || turn.cancelled) return finishCancelled(emit);
    if (action === 'decline') {
      await stream('message_chunk', 'Elicitation declined.');
      emit({ type: 'turn_complete', stop_reason: 'refusal' });
      return;
    }
    await stream('message_chunk', 'Elicitation accepted. Continuing.');
    emit({ type: 'turn_complete', stop_reason: 'end_turn' });
    return;
  }

  if (scenario !== 'tools') {
    await stream(
      'thought_chunk',
      'The user wants a change in the web layer. I read the router first, then the handler that owns the route.',
    );
    if (turn.cancelled) return finishCancelled(emit);
    await pause(250);
  }

  if (scenario === 'full' || scenario === 'plan' || scenario === 'long') {
    const steps = [
      { content: 'Read the router and the handlers', status: 'in_progress' },
      { content: 'Add the new route', status: 'pending' },
      { content: 'Write an integration test', status: 'pending' },
    ];
    emit({ type: 'plan', entries: steps });
    await pause(400);

    // A plan event replaces the plan, so progress shows in place.
    steps[0].status = 'completed';
    steps[1].status = 'in_progress';
    if (turn.cancelled) return finishCancelled(emit);
    emit({ type: 'plan', entries: steps.map((step) => ({ ...step })) });
    await pause(400);
  }

  if (scenario !== 'plan') {
    const reads = [
      { title: 'Read backend/src/web/mod.rs', kind: 'read', output: '203 lines. The router lists every route.', locations: [{ path: 'backend/src/web/mod.rs', line: 82 }] },
      { title: 'Grep "api/chats" backend/src/', kind: 'search', output: '7 matches in backend/src/web/hub.rs' },
    ];
    for (const read of reads) {
      if (turn.cancelled) return finishCancelled(emit);
      const toolId = randomUUID();
      emit({ type: 'tool_call', id: toolId, title: read.title, kind: read.kind, status: 'in_progress', locations: read.locations ?? null });
      await pause(500);
      emit({
        type: 'tool_call_update',
        id: toolId,
        title: read.title,
        kind: read.kind,
        status: 'completed',
        output: read.output,
      });
      await pause(150);
    }
  }

  if (scenario === 'permission' || scenario === 'full' || scenario === 'long') {
    if (turn.cancelled) return finishCancelled(emit);
    const optionId = await requestPermission(state, chat, turn, pause);
    if (optionId === null || turn.cancelled) return finishCancelled(emit);

    if (optionId.startsWith('write-reject')) {
      await stream('message_chunk', 'I stopped, because the edit was denied.');
      emit({ type: 'turn_complete', stop_reason: 'refusal' });
      return;
    }

    const toolId = randomUUID();
    emit({ type: 'tool_call', id: toolId, title: 'Edit backend/src/web/hub.rs', kind: 'edit', status: 'in_progress' });
    await pause(600);
    emit({
      type: 'tool_call_update',
      id: toolId,
      title: 'Edit backend/src/web/hub.rs',
      kind: 'edit',
      status: 'completed',
      output: '+18 -2',
    });
    await pause(150);
  }

  if (turn.cancelled) return finishCancelled(emit);

  const answer =
    scenario === 'long'
      ? LONG_ANSWER
      : 'I added the route and its handler.\n\n### Summary of Changes\n- The handler validates the path against configured project roots.\n- Integration tests cover the rejected case.\n- Stored procedures and schema migrations updated.';
  await stream('message_chunk', answer);

  if (turn.cancelled) return finishCancelled(emit);

  // A real agent renames the chat after the first turn.
  state.updateGeneratedTitle(chat, titleFor(text));

  emit({ type: 'turn_complete', stop_reason: 'end_turn' });
}

/** Emits an agent-owned permission request and waits for the browser. */
async function requestPermission(state, chat, turn, pause) {
  const requestId = randomUUID();
  turn.permissionId = requestId;
  turn.permissionOptionIds = new Set([
    'write-once',
    'write-always',
    'write-reject-once',
    'write-reject-always',
  ]);

  const answered = new Promise((resolve) => {
    turn.resolvePermission = resolve;
  });

  state.emit(chat.id, chat.agent, {
    type: 'permission_request',
    id: requestId,
    method: 'fs/write_text_file',
    title: 'Write backend/src/web/hub.rs',
    kind: 'edit',
    description: 'Write backend/src/web/hub.rs',
    options: [
      { optionId: 'write-once', name: 'Allow once', kind: 'allow_once' },
      { optionId: 'write-always', name: 'Always allow', kind: 'allow_always' },
      { optionId: 'write-reject-once', name: 'Reject once', kind: 'reject_once' },
      { optionId: 'write-reject-always', name: 'Always reject', kind: 'reject_always' },
    ],
  });


  const optionId = await answered;
  turn.permissionId = null;
  turn.resolvePermission = null;

  if (optionId === null || turn.cancelled) return null;
  state.emit(chat.id, chat.agent, {
    type: 'permission_response',
    id: requestId,
    option_id: optionId,
  });
  await pause(200);
  return optionId;
}

function finishCancelled(emit) {
  emit({ type: 'turn_complete', stop_reason: 'cancelled' });
}

/** Builds a chat title the way an agent does after the first turn. */
function titleFor(text) {
  const clean = text.trim().replace(/\s+/g, ' ');
  const short = clean.length > 48 ? `${clean.slice(0, 48)}…` : clean;
  return short.charAt(0).toUpperCase() + short.slice(1);
}

const LONG_ANSWER = [
  'The router now owns the new route, and the handler validates its input before it touches the store.',
  'I kept the validation in one place, because the clone endpoint and the create endpoint need the same rule.',
  'The path is canonicalized first, so a symlink cannot escape a configured project root.',
  'A rejected path returns 400 with the reason in the body, which the frontend shows in the dialog.',
  'The integration test drives the router through `oneshot`, so it needs no running server.',
  'I also removed the duplicated error mapping, which the two handlers had each written out.',
].join(' ');
