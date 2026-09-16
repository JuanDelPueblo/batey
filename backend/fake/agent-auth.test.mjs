import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

import { FakeAgentAuth } from './agent-auth.mjs';

describe('fake agent authentication', () => {
  it('mirrors the authentication view contract', () => {
    const auth = new FakeAgentAuth();
    const view = auth.agentView('claude');
    assert.equal(view.agent_id, 'claude');
    assert.equal(view.logout_supported, true);
    assert.equal(view.terminal_supported, true);
    assert.equal(view.observed_state, 'unknown');
    for (const method of view.methods) {
      assert.equal(typeof method.id, 'string');
      assert.equal(typeof method.name, 'string');
      assert.ok(['agent', 'terminal', 'browser-popup'].includes(method.type));
      assert.equal(typeof method.supported, 'boolean');
    }
    assert.equal(auth.agentView('unknown'), null);
  });

  it('reports an unknown method kind as unsupported', () => {
    const auth = new FakeAgentAuth();
    const method = auth.method('codex', 'codex-future');
    assert.equal(method.type, 'browser-popup');
    assert.equal(method.supported, false);
  });

  it('exposes a safe active flow for recovery', () => {
    const auth = new FakeAgentAuth();
    assert.equal(auth.agentView('claude').active_flow, null);
    const { flow } = auth.startFlow('claude', 'claude-terminal');
    const view = auth.agentView('claude');
    assert.equal(view.active_flow.kind, 'terminal');
    assert.equal(view.active_flow.flow_id, flow.flow_id);
    assert.equal(view.active_flow.method_id, 'claude-terminal');
    const serialized = JSON.stringify(view);
    assert.equal(serialized.includes('scrollback'), false);
    assert.equal(serialized.includes('"output"'), false);
  });

  it('bounds concurrent flows per agent and frees the slot on cancel', () => {
    const auth = new FakeAgentAuth();
    const { flow } = auth.startFlow('claude', 'claude-terminal');
    assert.equal(flow.state, 'running');
    assert.equal(flow.flow_id.length, 64);
    assert.ok(auth.startFlow('claude', 'claude-terminal').error);
    auth.cancel(flow);
    assert.equal(flow.state, 'cancelled');
    assert.ok(auth.startFlow('claude', 'claude-terminal').flow);
  });

  it('never puts terminal output in the flow summary', () => {
    const auth = new FakeAgentAuth();
    const { flow } = auth.startFlow('codex', 'codex-terminal');
    auth.emit(flow, 'a secret token\r\n');
    const view = auth.flowView(flow);
    assert.equal('scrollback' in view, false);
    assert.equal('listeners' in view, false);
    assert.equal(JSON.stringify(view).includes('secret token'), false);
  });

  it('succeeds on "ok" and fails on "fail"', () => {
    const auth = new FakeAgentAuth();
    const first = auth.startFlow('codex', 'codex-terminal').flow;
    auth.handleMessage(first, { type: 'input', data: 'ok\n' });
    assert.equal(first.state, 'succeeded');
    assert.equal(first.exit_code, 0);
    assert.ok(auth.authenticated.has('codex'));

    const second = auth.startFlow('claude', 'claude-terminal').flow;
    auth.handleMessage(second, { type: 'input', data: 'fail\n' });
    assert.equal(second.state, 'failed');
    assert.equal(second.exit_code, 3);
  });
});
