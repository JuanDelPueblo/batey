import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

import { FakeState } from './state.mjs';
import { isRunning, scenarioFor, startTurn } from './turns.mjs';

function historyFor(state, chatId) {
  return state.events.filter((event) => event.session_id === chatId);
}

async function waitFor(predicate) {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
  assert.fail('timed out waiting for fake turn');
}

describe('fake scripted turns', () => {
  it('keeps existing prompt keywords and adds the UI fixture scenarios', () => {
    assert.equal(scenarioFor('make a plan'), 'plan');
    assert.equal(scenarioFor('ask for permission'), 'permission');
    assert.equal(scenarioFor('please elicit my name'), 'elicit-form');
    assert.equal(scenarioFor('rich screenshot review'), 'rich');
    assert.equal(scenarioFor('run a terminal task'), 'terminal');
    assert.equal(scenarioFor('a long answer'), 'long');
    assert.equal(scenarioFor('make it error'), 'error');
    assert.equal(scenarioFor('streaming-markdown demo'), 'streaming-markdown');
  });

  it('creates rich output and a running terminal task through normal events', async () => {
    const state = new FakeState();
    const projectId = [...state.projects.values()].find((project) => project.name === 'scratch').id;
    const rich = state.createChat(projectId, 'opencode', 'Manual rich scenario');
    startTurn(state, rich, 'rich screenshot review', 0);
    await waitFor(() => !isRunning(rich.id));
    const richHistory = historyFor(state, rich.id);
    assert.ok(richHistory.some((event) => event.payload.type === 'message_chunk' && event.payload.content?.some((block) => block.type === 'image')));
    assert.ok(richHistory.some((event) => event.payload.type === 'tool_call' && Array.isArray(event.payload.content)));

    const terminal = state.createChat(projectId, 'antigravity', 'Manual terminal scenario');
    startTurn(state, terminal, 'run a terminal task', 0);
    await waitFor(() => !isRunning(terminal.id));
    const tasks = state.listTasks(terminal.id);
    assert.equal(tasks.length, 1);
    assert.equal(tasks[0].state, 'running');
    assert.ok(historyFor(state, terminal.id).some((event) => event.payload.type === 'turn_complete'));
  });

  it('records the error scenario as an ACP error and failed turn', async () => {
    const state = new FakeState();
    const projectId = [...state.projects.values()][0].id;
    const chat = state.createChat(projectId, 'claude', 'Manual error scenario');
    startTurn(state, chat, 'force an error', 0);
    await waitFor(() => !isRunning(chat.id));
    const history = historyFor(state, chat.id);
    assert.ok(history.some((event) => event.payload.type === 'error'));
    assert.ok(history.some((event) => event.payload.type === 'turn_complete' && event.payload.stop_reason === 'error'));
  });

  it('streams split Markdown as text deltas with stable message ids', async () => {
    const state = new FakeState();
    const projectId = [...state.projects.values()][0].id;
    const chat = state.createChat(projectId, 'codex', 'Manual streaming-markdown scenario');
    startTurn(state, chat, 'streaming-markdown demo', 0);
    await waitFor(() => !isRunning(chat.id));
    const history = historyFor(state, chat.id);
    const thought = history.filter((event) => event.payload.type === 'thought_chunk');
    assert.ok(thought.length > 1);
    assert.ok(thought.every((event) => event.payload.message_id === 'thought-stream-1'));
    assert.ok(thought.every((event) => event.payload.content?.[0]?.type === 'text'));
    const joined = thought.map((event) => event.payload.content[0].text).join('');
    assert.ok(joined.includes('**bold**'));
    const chunks = history.filter((event) => event.payload.type === 'message_chunk' && event.payload.message_id === 'msg-stream-1');
    assert.ok(chunks.length > 1);
    assert.equal(chunks.map((event) => event.payload.content[0].text).join(''), '## Summary\nThis fixes **bold** and `code`.\n```rust\nfn main() {}\n```\n- item 1\n- item 2\n');
    const mixed = history.filter((event) => event.payload.type === 'message_chunk' && event.payload.message_id === 'msg-stream-2');
    assert.equal(mixed.length, 3);
    assert.equal(mixed[0].payload.content[0].type, 'text');
    assert.equal(mixed[1].payload.content[0].type, 'image');
    assert.equal(mixed[2].payload.content[0].type, 'text');
  });
});
