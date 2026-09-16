import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

import { AGENTS, FakeState, RICH_HISTORY_CONTENT, validateCustomInput } from './state.mjs';
import { cancel, isRunning, seedActiveTurn } from './turns.mjs';

function historyFor(state, chatId) {
  return state.events.filter((event) => event.session_id === chatId);
}

function payloadTypes(history) {
  return history.map((event) => event.payload.type);
}

describe('fake backend seed history', () => {
  it('mirrors the extended agent summary contract', () => {
    for (const agent of AGENTS) {
      assert.ok(['editable', 'registry_managed', 'read_only'].includes(agent.mutability));
      assert.equal(typeof agent.display, 'object');
      if (agent.availability === 'unavailable') {
        assert.equal(typeof agent.unavailable_reason, 'string');
      } else {
        assert.equal('unavailable_reason' in agent, false);
      }
    }
  });

  it('reports structural custom-agent validation failures without mutation', () => {
    const report = validateCustomInput({ id: 'not valid', command: '' });
    assert.equal(report.valid, false);
    assert.deepEqual(report.issues.map((issue) => issue.field), ['id', 'command']);
  });
  it('seeds realistic idle, active, waiting, failed, task, blocked, rich, archived, and empty chats', () => {
    const state = new FakeState();
    const chats = [...state.chats.values()];

    assert.equal(chats.length, 10);

    for (const chat of chats) {
      const history = historyFor(state, chat.id);

      for (const event of history) {
        assert.equal(event.session_id, chat.id);
        assert.equal(event.agent, chat.agent);
      }
    }

    const byTitle = (title) => chats.find((chat) => chat.title === title);
    const completed = [
      'Review the WebSocket replay path',
      'Run the workspace verification suite',
      'Authorize the project .envrc before running tests',
      'Port the store to versioned migrations',
      'Inspect the Corolla calibration checksum',
      'Compare the dashboard render trace and screenshot',
    ];
    for (const title of completed) {
      const history = historyFor(state, byTitle(title).id);
      assert.ok(payloadTypes(history).includes('turn_complete'), `${title} should be complete`);
    }

    const working = byTitle('Trace the reconnect race in EventLog');
    assert.equal(state.chatView(working).turn_state, 'PROMPTING');
    assert.equal(state.chatView(working).process_state, 'RUNNING');
    assert.equal(payloadTypes(historyFor(state, working.id)).includes('turn_complete'), false);

    const waiting = byTitle('Approve the WebSocket backpressure fix');
    const waitingHistory = historyFor(state, waiting.id);
    assert.equal(state.chatView(waiting).turn_state, 'PROMPTING');
    assert.ok(waitingHistory.some((event) => event.payload.type === 'permission_request'));
    assert.equal(waitingHistory.some((event) => event.payload.type === 'permission_response'), false);
    assert.equal(waitingHistory.some((event) => event.payload.type === 'turn_complete'), false);

    const failed = byTitle('Recover the failed schema migration check');
    const failedHistory = historyFor(state, failed.id);
    assert.equal(state.chatView(failed).process_state, 'DEAD');
    assert.ok(failedHistory.some((event) => event.payload.type === 'error'));
    assert.equal(failedHistory.at(-2).payload.stop_reason, 'error');

    const terminal = byTitle('Run the workspace verification suite');
    assert.equal(state.chatView(terminal).active_tasks, 1);
    assert.equal(state.listTasks(terminal.id)[0].state, 'running');

    const blocked = byTitle('Authorize the project .envrc before running tests');
    assert.equal(state.isEnvironmentBlocked(blocked.id), true);

    const archived = byTitle('Port the store to versioned migrations');
    assert.equal(archived.archived, true);
    assert.equal(state.chatView(archived).process_state, 'STOPPED');

    const rich = byTitle('Compare the dashboard render trace and screenshot');
    const richEvents = historyFor(state, rich.id);
    const richBlocks = richEvents.flatMap((event) => event.payload.content ?? []);
    assert.ok(richBlocks.some((block) => block.type === 'image'));
    assert.ok(richBlocks.some((block) => block.type === 'resource_link'));
    assert.ok(richBlocks.some((block) => block.type === 'resource'));
    assert.equal(richBlocks.length >= RICH_HISTORY_CONTENT.length, true);

    const empty = byTitle('Draft a release checklist');
    assert.equal(historyFor(state, empty.id).length, 0);
    assert.equal(empty.acp_session_id, null);
  });

  it('replays the seeded history from seq 0 for a fresh page', () => {
    const state = new FakeState();
    const replayed = state.replayFrom(0);

    for (const chat of state.chats.values()) {
      const history = replayed.filter((event) => event.session_id === chat.id);
      const types = history.map((event) => event.payload.type);

      if (chat.title === 'Draft a release checklist') {
        assert.equal(history.length, 0);
      } else {
        assert.ok(types.includes('user_message'), `replay misses user_message for "${chat.title}"`);
        if (!['Approve the WebSocket backpressure fix', 'Recover the failed schema migration check'].includes(chat.title)) {
          assert.ok(types.includes('message_chunk'), `replay misses message_chunk for "${chat.title}"`);
        }
      }
    }
  });

  it('keeps seeded open turns live instead of completing them at startup', async () => {
    const state = new FakeState();
    const working = [...state.chats.values()].find((chat) => chat.title === 'Trace the reconnect race in EventLog');
    const waiting = [...state.chats.values()].find((chat) => chat.title === 'Approve the WebSocket backpressure fix');

    seedActiveTurn(state, working, state.seededTurns.get(working.id));
    seedActiveTurn(state, waiting, state.seededTurns.get(waiting.id));
    assert.equal(isRunning(working.id), true);
    assert.equal(isRunning(waiting.id), true);
    assert.equal(state.chatView(working).turn_state, 'PROMPTING');
    assert.equal(state.chatView(waiting).turn_state, 'PROMPTING');

    cancel(working.id);
    cancel(waiting.id);
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(isRunning(working.id), false);
    assert.equal(isRunning(waiting.id), false);
  });

  it('maps seeded event/runtime/task contracts to the four frontend activities', () => {
    const state = new FakeState();
    const activityFromContract = (chat) => {
      const history = historyFor(state, chat.id);
      const request = [...history].reverse().find((event) => event.payload.type === 'permission_request');
      const answered = request && history.some((event) => event.payload.type === 'permission_response' && event.payload.id === request.payload.id);
      if (request && !answered && !history.some((event) => event.payload.type === 'turn_complete' && event.seq > request.seq)) return 'waiting';
      if (history.some((event) => event.payload.type === 'error') || history.some((event) => event.payload.type === 'turn_complete' && event.payload.stop_reason === 'error')) return 'error';
      const view = state.chatView(chat);
      if (view.turn_state === 'PROMPTING' || view.turn_state === 'CANCELLING' || view.active_tasks > 0) return 'working';
      return 'idle';
    };

    const activity = (title) => activityFromContract([...state.chats.values()].find((chat) => chat.title === title));
    assert.equal(activity('Review the WebSocket replay path'), 'idle');
    assert.equal(activity('Trace the reconnect race in EventLog'), 'working');
    assert.equal(activity('Approve the WebSocket backpressure fix'), 'waiting');
    assert.equal(activity('Recover the failed schema migration check'), 'error');
  });

  it('exposes seeded runtime metadata through the project chat-list contract', () => {
    const state = new FakeState();
    const listed = [...state.projects.values()].flatMap((project) => state.listChats(project.id));
    assert.equal(listed.length, 10);
    assert.ok(listed.some((chat) => chat.turn_state === 'PROMPTING' && chat.process_state === 'RUNNING'));
    assert.ok(listed.some((chat) => chat.process_state === 'DEAD' && chat.turn_state === 'IDLE'));
    assert.ok(listed.some((chat) => chat.active_tasks === 1));
    assert.ok(listed.every((chat) => typeof chat.created_at === 'string' && typeof chat.updated_at === 'string'));
    assert.ok(listed.every((chat) => !('workspace_path' in chat) && !('repository_root' in chat)));
    assert.deepEqual(
      listed.filter((chat) => chat.title === 'Trace the reconnect race in EventLog').map((chat) => chat.config_values),
      [{ model: 'gpt-5' }],
    );
  });

  it('returns bounded, chat-scoped pages with stable older cursors', () => {
    const state = new FakeState();
    const chat = [...state.chats.values()][0];
    const all = historyFor(state, chat.id);
    const first = state.historyPage(chat.id, undefined, 2);
    assert.equal(first.events.length, 2);
    assert.equal(first.has_older, true);
    assert.deepEqual(first.events.map((event) => event.seq), all.slice(-2).map((event) => event.seq));

    const older = state.historyPage(chat.id, first.next_cursor, 2);
    assert.ok(older.events.every((event) => event.session_id === chat.id));
    assert.ok(older.events.at(-1).seq < first.events[0].seq);

    const bounded = state.historyPage(chat.id, undefined, 10, all.at(-3).seq);
    assert.ok(bounded.events.every((event) => event.seq <= all.at(-3).seq));
    assert.equal(bounded.has_older, false);
  });

  it('keeps history for the archived seed chat and none for a new chat', () => {
    const state = new FakeState();
    const archived = [...state.chats.values()].find((chat) => chat.archived);
    assert.ok(archived, 'expected one archived seed chat');
    const archivedTypes = payloadTypes(historyFor(state, archived.id));
    assert.ok(archivedTypes.includes('user_message'));
    assert.ok(archivedTypes.includes('message_chunk'));
    assert.ok(archivedTypes.includes('turn_complete'));

    const projectId = [...state.projects.values()][0].id;
    const fresh = state.createChat(projectId, 'codex');
    assert.equal(historyFor(state, fresh.id).length, 0);
  });

  it('numbers default titles without reusing deleted numbers', () => {
    const state = new FakeState();
    const projectId = [...state.projects.values()][0].id;
    const first = state.createChat(projectId, 'codex');
    const second = state.createChat(projectId, 'codex');

    assert.equal(first.title, 'New chat 1');
    assert.equal(second.title, 'New chat 2');
    assert.equal(first.title_overridden, false);
    assert.equal(second.title_overridden, false);

    state.chats.delete(first.id);
    const third = state.createChat(projectId, 'codex');
    assert.equal(third.title, 'New chat 3');
    assert.equal(third.title_overridden, false);
  });

  it('lets generated titles update until a manual title wins', () => {
    const state = new FakeState();
    const projectId = [...state.projects.values()][0].id;
    const chat = state.createChat(projectId, 'codex');

    assert.equal(state.updateGeneratedTitle(chat, 'Generated title'), true);
    assert.equal(chat.title, 'Generated title');
    chat.title = 'Manual title';
    chat.title_overridden = true;
    assert.equal(state.updateGeneratedTitle(chat, 'Later generated title'), false);
    assert.equal(chat.title, 'Manual title');
  });

  it('provides Git and non-Git workspace options and retains selections', () => {
    const state = new FakeState();
    const gitProject = [...state.projects.values()].find((project) => project.name === 'batey');
    const nonGitProject = [...state.projects.values()].find((project) => project.name === 'scratch');
    const options = state.workspaceOptions(gitProject.id);
    assert.equal(options.is_git, true);
    assert.equal(options.current_branch, 'master');
    assert.equal(options.dirty, true);
    assert.ok(options.branches.some((branch) => branch.name === 'feature/ui'));
    assert.equal(state.workspaceOptions(nonGitProject.id).is_git, false);

    const chat = state.createChat(gitProject.id, 'codex', undefined, {
      mode: 'project_checkout', branch: 'feature/ui',
    });
    assert.deepEqual(chat.workspace, {
      mode: 'project_checkout',
      branch: 'feature/ui',
      base_commit: '2222222222222222222222222222222222222222',
    });
  });

  it('sorts chat collections by activity newest first with a deterministic tie-breaker', () => {
    const state = new FakeState();
    const projectId = [...state.projects.values()][0].id;
    const older = state.createChat(projectId, 'codex', 'Older');
    const newer = state.createChat(projectId, 'codex', 'Newer');
    state.touchChatActivity(older.id, '2026-01-01T00:00:00.000Z');
    state.touchChatActivity(newer.id, '2026-02-01T00:00:00.000Z');
    assert.deepEqual(
      state.listChats(projectId).slice(-2).map((chat) => chat.id),
      [newer.id, older.id],
    );
  });

  it('records prompt activity at the durable user-message timestamp', () => {
    const state = new FakeState();
    const projectId = [...state.projects.values()][0].id;
    const chat = state.createChat(projectId, 'codex', 'Prompt activity');
    const event = state.emit(chat.id, chat.agent, { type: 'user_message', text: 'Hello' });
    state.touchChatActivity(chat.id, event.timestamp);
    assert.equal(chat.updated_at, event.timestamp);
  });

  it('derives an active turn start independently of the history page', () => {
    const state = new FakeState();
    const projectId = [...state.projects.values()][0].id;
    const chat = state.createChat(projectId, 'codex', 'Long turn');
    const user = state.emit(chat.id, chat.agent, { type: 'user_message', text: 'Working' });
    state.touchChatActivity(chat.id, user.timestamp);
    state.emit(chat.id, chat.agent, { type: 'message_chunk', text: 'Still working' });
    state.setRuntime(chat.id, 'RUNNING', 'PROMPTING');

    assert.equal(state.chatView(chat).turn_started_at, user.timestamp);

    state.emit(chat.id, chat.agent, { type: 'turn_complete', stop_reason: 'end_turn' });
    assert.equal(state.chatView(chat).turn_started_at, null);
  });

  it('seeds representative managed and direct workspace summaries without paths', () => {
    const state = new FakeState();
    const gitProject = [...state.projects.values()].find((project) => project.name === 'batey');
    const chats = state.listChats(gitProject.id);
    const managed = chats.find((chat) => chat.workspace?.mode === 'managed_worktree');
    const direct = chats.find((chat) => chat.workspace?.mode === 'project_checkout');
    assert.ok(managed?.workspace?.branch?.startsWith(`batey/chat/${managed.id}`));
    assert.equal(direct?.workspace?.branch, 'feature/ui');
    for (const chat of [managed, direct]) {
      assert.ok(chat);
      assert.equal('workspace_path' in chat.workspace, false);
      assert.equal('repository_root' in chat.workspace, false);
    }
  });
  it('tracks terminal tasks and reflects active count in chat view', () => {
    const state = new FakeState();
    const projectId = [...state.projects.values()][0].id;
    const chat = state.createChat(projectId, 'antigravity');

    assert.equal(state.chatView(chat).active_tasks, 0);
    assert.deepEqual(state.listTasks(chat.id), []);

    const task1 = state.createTask(chat.id, 'cargo test', '/home/dev/projects/batey', 'running tests...');
    assert.equal(task1.state, 'running');
    assert.equal(state.chatView(chat).active_tasks, 1);

    const task2 = state.createTask(chat.id, 'npm run build', '/home/dev/projects/batey/frontend');
    assert.equal(state.chatView(chat).active_tasks, 2);

    const tasks = state.listTasks(chat.id);
    assert.equal(tasks.length, 2);
    assert.equal('output' in tasks[0], false); // listTasks omits output

    const detail1 = state.getTask(chat.id, task1.id);
    assert.equal(detail1.output, 'running tests...');

    state.completeTask(chat.id, task1.id, 0, 'passed');
    assert.equal(state.getTask(chat.id, task1.id).state, 'completed');
    assert.equal(state.chatView(chat).active_tasks, 1);

    state.stopTask(chat.id, task2.id);
    assert.equal(state.getTask(chat.id, task2.id).state, 'stopped');
    assert.equal(state.chatView(chat).active_tasks, 0);
  });

  it('manages blocked direnv workspace state and authorization', () => {
    const state = new FakeState();
    const projectId = [...state.projects.values()][0].id;
    const chat = state.createChat(projectId, 'codex');

    assert.equal(state.isEnvironmentBlocked(chat.id), false);
    state.blockEnvironment(chat.id);
    assert.equal(state.isEnvironmentBlocked(chat.id), true);
    state.authorizeEnvironment(chat.id);
    assert.equal(state.isEnvironmentBlocked(chat.id), false);
  });

  it('remembers a project-level direnv grant only when asked', () => {
    const state = new FakeState();
    const project = state.createProject('grant-test', '/tmp/grant-test');
    const chat = state.createChat(project.id, 'codex');

    assert.equal(state.hasProjectEnvrcGrant(project.id), false);
    assert.equal(state.projectView(project).envrc_remembered, false);

    state.blockEnvironment(chat.id);
    state.authorizeEnvironment(chat.id);
    assert.equal(
      state.hasProjectEnvrcGrant(project.id),
      false,
      'a plain "allow this workspace" must never remember the project',
    );

    state.blockEnvironment(chat.id);
    state.authorizeEnvironment(chat.id, true);
    assert.equal(state.hasProjectEnvrcGrant(project.id), true);
    const view = state.projectView(project);
    assert.equal(view.envrc_remembered, true);
    assert.equal(view.envrc_relative_path, '.envrc');

    state.forgetProjectEnvrc(project.id);
    assert.equal(state.hasProjectEnvrcGrant(project.id), false);
    assert.equal(state.projectView(project).envrc_remembered, false);
  });

  it('does not block a new chat under a project with a matching remembered grant', () => {
    const state = new FakeState();
    const project = state.createProject('remembered-project', '/tmp/remembered-project');
    const first = state.createChat(project.id, 'codex');
    state.blockEnvironment(first.id);
    state.authorizeEnvironment(first.id, true);
    assert.equal(state.hasProjectEnvrcGrant(project.id), true);

    const second = state.createChat(project.id, 'codex');
    state.blockEnvironmentUnlessRemembered(second.id, project.id);
    assert.equal(state.isEnvironmentBlocked(second.id), false);

    state.forgetProjectEnvrc(project.id);
    const third = state.createChat(project.id, 'codex');
    state.blockEnvironmentUnlessRemembered(third.id, project.id);
    assert.equal(state.isEnvironmentBlocked(third.id), true);
  });

  it('derives registry installed state from the one agent catalog', () => {
    const state = new FakeState();
    const view = state.registryView();
    assert.equal(view.status, 'fresh');
    const installed = view.agents.find((entry) => entry.id === 'example-acp');
    assert.equal(installed.installed_as, 'example-acp');
    assert.equal(installed.installed_version, '1.0.0');
    assert.equal(installed.update_available, true);

    const unsupported = view.agents.find((entry) => entry.id === 'windows-only');
    assert.equal(typeof unsupported.unsupported_reason, 'string');
    assert.equal('installed_as' in unsupported, false);

    assert.deepEqual(state.registryView('native').agents.map((entry) => entry.id), ['native-agent']);
    assert.equal(state.registryView().status, 'cached');
    assert.equal(state.registryView('', true).status, 'fresh');
  });

  it('keeps the full cached catalog and timestamp after a failed refresh', () => {
    const state = new FakeState();
    const first = state.registryView();
    assert.ok(first.agents.length > 0);
    assert.deepEqual(first.rejected, []);
    state.registryView('native');
    assert.deepEqual(state.registryView().agents, first.agents);
    state.registryRefreshError = 'Network is unreachable';
    const failed = state.registryView('', true);
    assert.equal(failed.status, 'cached');
    assert.equal(failed.fetched_at, first.fetched_at);
    assert.equal(failed.error, 'Network is unreachable');
    assert.deepEqual(failed.agents, first.agents);
    state.registryRefreshError = null;
    assert.equal(state.registryView('', true).status, 'fresh');
  });

  it('reports an unavailable registry when the first fetch fails', () => {
    const state = new FakeState();
    state.registryRefreshError = 'Network is unreachable';
    const failed = state.registryView();
    assert.equal(failed.status, 'unavailable');
    assert.deepEqual(failed.agents, []);
    assert.equal(failed.fetched_at, undefined);
    state.registryRefreshError = null;
    assert.ok(state.registryView('', true).agents.length > 0);
  });

  it('installs, updates, and uninstalls registry agents', () => {
    const state = new FakeState();
    const installed = state.installRegistryAgent({ registry_id: 'native-agent' });
    assert.equal(installed.source, 'registry');
    assert.equal(installed.display.version, '2.0.0');

    const outcome = state.updateRegistryAgent('example-acp');
    assert.equal(outcome.updated, true);
    assert.equal(outcome.to_version, '1.2.0');

    const removal = state.removeAgent('example-acp');
    assert.equal(removal.deleted, true);
    assert.equal(state.agent('example-acp'), undefined);

    state.removeAgent(installed.id);
    assert.equal(state.agent(installed.id), undefined);
  });

  it('exposes authenticated custom detail and edits a Batey-managed agent', () => {
    const state = new FakeState();
    const detail = state.agentDetail('my-custom');
    assert.equal(detail.command, 'my-agent');
    assert.deepEqual(detail.args, ['--acp']);
    assert.equal(detail.env.MY_AGENT_TOKEN, 'fake-token');

    const edited = state.editCustomAgent('my-custom', {
      id: 'my-custom', command: 'my-agent', args: [], env: {}, display_name: 'Renamed',
    });
    assert.equal(edited.display_name, 'Renamed');
    assert.equal(state.agentDetail('my-custom').display_name, 'Renamed');

    assert.throws(() => state.agentDetail('codex'), /not an editable/);
    assert.throws(() => state.removeAgent('codex'), /read-only/);
  });

  it('manages private per-agent environment with Keep/Replace/Remove redaction', () => {
    const state = new FakeState();
    assert.deepEqual(state.agentEnvPresence('my-custom'), []);
    const afterCreate = state.applyAgentEnvEdits('my-custom', [
      { name: 'CODEX_API_KEY', value: 'secret', action: 'replace' },
      { name: 'NO_BROWSER', value: '1', action: 'replace' },
    ]);
    assert.deepEqual(afterCreate, [
      { name: 'CODEX_API_KEY', present: true },
      { name: 'NO_BROWSER', present: true },
    ]);
    assert.ok(!JSON.stringify(afterCreate).includes('secret'));

    const afterEdit = state.applyAgentEnvEdits('my-custom', [
      { name: 'CODEX_API_KEY', action: 'keep' },
      { name: 'CODEX_API_KEY', value: 'second', action: 'replace' },
    ]);
    // The second edit wins sequentially; presence stays redacted.
    assert.ok(afterEdit.some((entry) => entry.name === 'CODEX_API_KEY'));
    assert.ok(!JSON.stringify(afterEdit).includes('second'));

    state.applyAgentEnvEdits('my-custom', [{ name: 'NO_BROWSER', action: 'remove' }]);
    assert.deepEqual(state.agentEnvPresence('my-custom').map((entry) => entry.name), ['CODEX_API_KEY']);

    assert.throws(() => state.agentEnvPresence('codex'), /not an installed agent/);
    assert.throws(() => state.applyAgentEnvEdits('my-custom', [{ name: 'HAS-DASH', value: 'x' }]), /unusable name/);
    assert.throws(() => state.applyAgentEnvEdits('my-custom', [{ name: 'NEW', action: 'keep' }]), /unknown/);

    // Uninstall cleans up overrides.
    state.removeAgent('my-custom');
    assert.equal(state.agent('my-custom'), undefined);
  });

  it('reports provider-neutral authentication state', () => {
    const state = new FakeState();
    const codex = state.agentAuth('codex');
    assert.equal(codex.agent_id, 'codex');
    assert.equal(codex.logout_supported, true);
    assert.equal(codex.terminal_supported, true);
    assert.equal(codex.observed_state, 'unknown');
    assert.equal(codex.methods.length, 2);
    assert.equal(codex.methods[0].id, 'openai-oauth');
    assert.equal(codex.methods[0].type, 'agent');
    assert.equal(codex.methods[0].supported, true);
    assert.equal(codex.methods[1].id, 'api-key');
    assert.equal(codex.methods[1].type, 'terminal');
    assert.equal(codex.methods[1].supported, true);

    const opencode = state.agentAuth('opencode');
    assert.equal(opencode.logout_supported, false);
    assert.equal(opencode.observed_state, 'unknown');
    assert.equal(opencode.methods[1].type, 'device_code');
    assert.equal(opencode.methods[1].supported, false);

    const afterLogin = state.authenticateAgent('codex', 'openai-oauth');
    assert.equal(afterLogin.agent_id, 'codex');
    assert.equal(afterLogin.observed_state, 'authenticated');

    const afterLogout = state.logoutAgent('codex');
    assert.equal(afterLogout.agent_id, 'codex');
    assert.equal(afterLogout.observed_state, 'authentication_required');

    assert.throws(() => state.authenticateAgent('codex', 'missing'), /Unknown authentication method/);
    assert.throws(() => state.authenticateAgent('opencode', 'device-code'), /unsupported/i);
    assert.throws(() => state.authenticateAgent('codex', 'api-key'), /terminal/i);
    assert.throws(() => state.logoutAgent('opencode'), /does not support logout/);
  });

  it('T140: reports freshness truthfully across an explicit refresh and a plain read', () => {
    const state = new FakeState();

    // Never checked yet.
    const initial = state.agentAuth('codex');
    assert.equal(initial.freshness, 'unknown');
    assert.equal(initial.checked_at, null);

    // An explicit refresh is the direct result of a live check.
    const refreshed = state.refreshAgentAuth('codex');
    assert.equal(refreshed.freshness, 'fresh');
    assert.ok(refreshed.checked_at);

    // A later plain read answers from the cache, not as a fresh check.
    const cached = state.agentAuth('codex');
    assert.equal(cached.freshness, 'cached');
    assert.equal(cached.checked_at, refreshed.checked_at);

    // Authenticating is itself a live check, so its own response is fresh,
    // and a later plain read is cached again.
    const afterAuth = state.authenticateAgent('codex', 'openai-oauth');
    assert.equal(afterAuth.freshness, 'fresh');
    assert.equal(state.agentAuth('codex').freshness, 'cached');

    assert.throws(() => state.refreshAgentAuth('missing-agent'), /Agent not found/);
  });

  it('T140: mutations mark the cache stale without erasing it, and removal forgets it', () => {
    const state = new FakeState();
    state.refreshAgentAuth('my-custom');
    assert.equal(state.agentAuth('my-custom').freshness, 'cached');

    // An environment change marks the cache stale.
    state.applyAgentEnvEdits('my-custom', [{ name: 'NO_BROWSER', value: '1', action: 'replace' }]);
    assert.equal(state.agentAuth('my-custom').freshness, 'stale');

    // A fresh check clears the stale marker again.
    state.refreshAgentAuth('my-custom');
    assert.equal(state.agentAuth('my-custom').freshness, 'cached');

    // An edited definition marks the cache stale too.
    state.editCustomAgent('my-custom', {
      id: 'my-custom',
      display_name: 'My Custom',
      command: 'my-agent',
      args: [],
      env: {},
    });
    assert.equal(state.agentAuth('my-custom').freshness, 'stale');

    // Removing the agent forgets the cache entirely, rather than leaving a
    // stale row nothing can ever refresh again.
    state.removeAgent('my-custom');
    assert.equal(state.authCheckedAt.has('my-custom'), false);
  });

  it('exposes safe active-flow discovery without private material', () => {
    const state = new FakeState();
    const terminal = state.startTerminalFlow('codex', 'api-key');
    const codex = state.agentAuth('codex');
    assert.equal(codex.active_flow.kind, 'terminal');
    assert.equal(codex.active_flow.flow_id, terminal.flow_id);
    assert.equal(codex.active_flow.method_id, 'api-key');
    assert.ok(codex.active_flow.started_at);

    // The seeded Antigravity protocol flow is discoverable too.
    const anti = state.agentAuth('antigravity');
    assert.equal(anti.active_flow.kind, 'protocol');
    assert.equal(anti.active_flow.method_id, 'antigravity-interactive');
    assert.equal(anti.active_flow.state, 'waiting_for_user');

    const serialized = JSON.stringify({ codex, anti });
    assert.equal(serialized.includes('scrollback'), false);
    assert.equal(serialized.includes('"output"'), false);
    assert.equal(serialized.includes('example.invalid'), false);
    assert.equal(serialized.includes('ABCD-1234'), false);
  });

  it('scopes the antigravity headless warning to its own method', () => {
    const state = new FakeState();
    const anti = state.agentAuth('antigravity');
    assert.ok(anti.methods[0].warning.includes('localhost'));
    assert.ok(anti.methods[0].warning.includes('GEMINI_API_KEY'));
    const codex = state.agentAuth('codex');
    assert.ok(codex.methods.every((method) => method.warning === undefined));
  });

  it('runs an async protocol flow with a URL elicitation', () => {
    const state = new FakeState();
    const flow = state.startProtocolFlow('codex', 'openai-oauth');
    assert.equal(flow.state, 'waiting_for_user');
    assert.equal(flow.flow_id.length, 64);
    const elicitations = state.listProtocolElicitations(flow.flow_id);
    assert.equal(elicitations.length, 1);
    assert.equal(elicitations[0].mode, 'url');
    assert.ok(elicitations[0].url.includes('example.invalid'));
    assert.ok(elicitations[0].url.includes('ABCD-1234'));

    state.respondProtocolElicitation(flow.flow_id, elicitations[0].id, 'accept', null);
    const view = state.protocolFlowView(flow.flow_id);
    assert.equal(view.state, 'succeeded');
    assert.equal(state.agentAuth('codex').observed_state, 'authenticated');
  });

  it('cancels a protocol flow without sticking in running', () => {
    const state = new FakeState();
    const flow = state.startProtocolFlow('codex', 'openai-oauth');
    const cancelled = state.cancelProtocolFlow(flow.flow_id);
    assert.equal(cancelled.state, 'cancelled');
    assert.equal(state.protocolFlowView(flow.flow_id).state, 'cancelled');
  });

  function attach(state, flowId) {
    const sent = [];
    const socket = {
      open: true,
      send: (text) => sent.push(JSON.parse(text)),
      close() { this.open = false; },
      onMessage: () => {},
      onClose: () => {},
    };
    state.attachFlowSocket(flowId, socket);
    return sent;
  }

  it('runs a terminal authentication flow over the flow socket', () => {
    const state = new FakeState();
    const flow = state.startTerminalFlow('codex', 'api-key');
    assert.equal(flow.state, 'running');
    assert.equal(flow.reason, null);

    const view = state.flowView(flow.flow_id);
    assert.equal(view.flow_id, flow.flow_id);
    assert.equal(view.agent_id, 'codex');
    assert.equal(view.method_id, 'api-key');
    assert.equal(view.state, 'running');
    assert.equal(view.method_name, undefined);

    const sent = attach(state, flow.flow_id);
    assert.equal(sent[0].type, 'output');
    assert.equal(sent[1].type, 'state');
    assert.equal(sent[1].state, 'running');

    state.flowResize(flow.flow_id, 120, 40);
    state.flowInput(flow.flow_id, 'secret-token');
    assert.ok(sent.some((message) => message.type === 'output' && message.data.includes('secret-token')));

    state.flowInput(flow.flow_id, '\r');
    const terminal = sent.at(-1);
    assert.equal(terminal.type, 'state');
    assert.equal(terminal.state, 'succeeded');
    assert.equal(terminal.exit_code, 0);
    assert.equal(state.flowView(flow.flow_id).state, 'succeeded');
  });

  it('supports failure, cancellation, and timeout terminal flows', () => {
    const state = new FakeState();
    const failing = state.startTerminalFlow('codex', 'api-key');
    const sent = attach(state, failing.flow_id);
    state.flowInput(failing.flow_id, 'fail\r');
    assert.equal(state.flowView(failing.flow_id).state, 'failed');
    assert.equal(state.flowView(failing.flow_id).reason, 'The authentication command failed.');
    assert.equal(sent.at(-1).state, 'failed');
    assert.equal(sent.at(-1).reason, 'The authentication command failed.');

    const timedOut = state.startTerminalFlow('codex', 'api-key');
    attach(state, timedOut.flow_id);
    state.flowInput(timedOut.flow_id, 'timeout\r');
    assert.equal(state.flowView(timedOut.flow_id).state, 'timed_out');
    assert.equal(state.flowView(timedOut.flow_id).reason, 'The authentication flow timed out.');

    const cancelled = state.startTerminalFlow('codex', 'api-key');
    attach(state, cancelled.flow_id);
    const view = state.cancelFlow(cancelled.flow_id);
    assert.equal(view.state, 'cancelled');
    assert.throws(() => state.cancelFlow('missing-flow'), /not found/);
  });

  it('rejects terminal flows for non-terminal methods', () => {
    const state = new FakeState();
    assert.throws(() => state.startTerminalFlow('codex', 'openai-oauth'), /not a terminal method/);
    assert.throws(() => state.startTerminalFlow('codex', 'missing'), /Unknown authentication method/);
  });
});
