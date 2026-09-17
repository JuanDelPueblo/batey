#!/usr/bin/env node
// Fake Batey backend.
//
// It serves the REST and WebSocket surface of `backend/src/web/` from memory, so the
// frontend runs without a Rust build and without an ACP agent binary. It is a
// development tool only. It has no persistence, no authentication and no
// access to the real filesystem.
//
// Usage: node fake-backend/server.mjs [--port 8765] [--latency 1.0]

import { createServer } from 'node:http';
import { randomUUID } from 'node:crypto';

import { upgrade } from './websocket.mjs';
import { AGENTS, FakeState, PROJECT_ROOT, defaultConfigOptions, validateCustomInput } from './state.mjs';
import { answerElicitation, answerPermission, cancel, isRunning, listPendingElicitations, seedActiveTurn, startTurn } from './turns.mjs';

const options = parseArgs(process.argv.slice(2));
const state = new FakeState();
state.seedAuthScenarios();
for (const [chatId, seed] of state.seededTurns) {
  const chat = state.chats.get(chatId);
  if (chat) seedActiveTurn(state, chat, seed);
}

// ---------------------------------------------------------------- routing

/** Route table. The patterns mirror the axum router of `backend/src/web/mod.rs`. */
const routes = [
  ['GET', /^\/api\/projects$/, () => json(state.listProjects())],
  ['POST', /^\/api\/projects$/, createProject],
  ['POST', /^\/api\/projects\/clone$/, cloneProject],
  ['GET', /^\/api\/filesystem\/directories$/, listDirectories],
  ['PATCH', /^\/api\/projects\/([^/]+)$/, editProject],
  ['DELETE', /^\/api\/projects\/([^/]+)$/, deleteProject],
  ['GET', /^\/api\/projects\/([^/]+)\/chats$/, listChats],
  ['GET', /^\/api\/projects\/([^/]+)\/workspace-options$/, workspaceOptions],
  ['POST', /^\/api\/projects\/([^/]+)\/chats$/, createChat],
  ['DELETE', /^\/api\/projects\/([^/]+)\/envrc-grant$/, forgetProjectEnvrcGrant],
  ['GET', /^\/api\/chats\/([^/]+)$/, getChat],
  ['GET', /^\/api\/chats\/([^/]+)\/history$/, history],
  ['PATCH', /^\/api\/chats\/([^/]+)$/, editChat],
  ['DELETE', /^\/api\/chats\/([^/]+)$/, deleteChat],
  ['POST', /^\/api\/chats\/([^/]+)\/prompt$/, promptChat],
  ['POST', /^\/api\/chats\/([^/]+)\/cancel$/, cancelChat],
  ['POST', /^\/api\/chats\/([^/]+)\/resume$/, resumeChat],
  ['POST', /^\/api\/chats\/([^/]+)\/stop$/, stopChat],
  ['POST', /^\/api\/chats\/([^/]+)\/permission$/, respondPermission],
  ['GET', /^\/api\/chats\/([^/]+)\/config$/, getConfig],
  ['PATCH', /^\/api\/chats\/([^/]+)\/config$/, setConfig],
  ['DELETE', /^\/api\/chats\/([^/]+)\/config\/([^/]+)$/, clearConfig],
  ['GET', /^\/api\/chats\/([^/]+)\/remote-sessions$/, remoteSessions],
  ['DELETE', /^\/api\/chats\/([^/]+)\/remote-sessions\/([^/]+)$/, deleteRemoteSession],
  ['GET', /^\/api\/chats\/([^/]+)\/commands$/, getCommands],
  ['GET', /^\/api\/chats\/([^/]+)\/modes$/, getModes],
  ['PATCH', /^\/api\/chats\/([^/]+)\/modes$/, setMode],
  ['GET', /^\/api\/chats\/([^/]+)\/usage$/, getUsage],
  ['GET', /^\/api\/chats\/([^/]+)\/session-info$/, getSessionInfo],
  ['GET', /^\/api\/chats\/([^/]+)\/mcp-servers$/, getMcpServers],
  ['POST', /^\/api\/chats\/([^/]+)\/mcp-servers$/, createMcpServer],
  ['PUT', /^\/api\/chats\/([^/]+)\/mcp-servers\/order$/, orderMcpServers],
  ['PATCH', /^\/api\/chats\/([^/]+)\/mcp-servers\/([^/]+)$/, editMcpServer],
  ['DELETE', /^\/api\/chats\/([^/]+)\/mcp-servers\/([^/]+)$/, deleteMcpServer],
  ['GET', /^\/api\/chats\/([^/]+)\/additional-roots$/, getAdditionalRoots],
  ['PUT', /^\/api\/chats\/([^/]+)\/additional-roots$/, setAdditionalRoots],
  ['GET', /^\/api\/chats\/([^/]+)\/elicitations$/, listElicitations],
  ['POST', /^\/api\/chats\/([^/]+)\/elicitations\/([^/]+)\/respond$/, respondElicitation],
  ['POST', /^\/api\/chats\/([^/]+)\/environment\/authorize$/, authorizeEnvironment],
  ['GET', /^\/api\/chats\/([^/]+)\/tasks$/, listTasks],
  ['GET', /^\/api\/chats\/([^/]+)\/tasks\/([^/]+)$/, getTask],
  ['POST', /^\/api\/chats\/([^/]+)\/tasks\/([^/]+)\/stop$/, stopTask],
  ['GET', /^\/api\/agents$/, () => json(AGENTS)],
  ['POST', /^\/api\/agents$/, createAgent],
  ['POST', /^\/api\/agents\/validate$/, validateAgent],
  ['GET', /^\/api\/agents\/registry$/, registryAgents],
  ['POST', /^\/api\/agents\/registry\/refresh$/, refreshRegistry],
  ['POST', /^\/api\/agents\/registry\/install$/, installRegistryAgent],
  ['POST', /^\/api\/agents\/([^/]+)\/update$/, updateRegistryAgent],
  ['GET', /^\/api\/agent-operations$/, listAgentOperations],
  ['GET', /^\/api\/agent-operations\/([^/]+)$/, getAgentOperation],
  // T111 authentication routes. The dedicated agent-auth surface is separate
  // from the per-agent routes, and the flow id stays opaque to the browser.
  // Protocol flows carry request-scoped elicitations, never durable chat events.
  ['GET', /^\/api\/agents\/([^/]+)\/auth$/, getAgentAuth],
  // The refresh route sits above the generic method-id route below, so a
  // refresh call never matches as a method named "refresh".
  ['POST', /^\/api\/agents\/([^/]+)\/auth\/refresh$/, refreshAgentAuth],
  ['POST', /^\/api\/agents\/([^/]+)\/auth\/terminal\/([^/]+)$/, startTerminalAuth],
  ['POST', /^\/api\/agents\/([^/]+)\/auth\/protocol\/([^/]+)$/, startProtocolAuth],
  ['POST', /^\/api\/agents\/([^/]+)\/auth\/([^/]+)$/, authenticateAgentRoute],
  ['POST', /^\/api\/agents\/([^/]+)\/logout$/, logoutAgentRoute],
  ['GET', /^\/api\/agent-auth\/([^/]+)$/, getAgentAuthFlow],
  ['POST', /^\/api\/agent-auth\/([^/]+)\/cancel$/, cancelAgentAuthFlow],
  ['GET', /^\/api\/agents\/([^/]+)\/environment$/, getAgentEnv],
  ['PATCH', /^\/api\/agents\/([^/]+)\/environment$/, updateAgentEnv],
  ['GET', /^\/api\/protocol-auth\/([^/]+)$/, getProtocolAuthFlow],
  ['POST', /^\/api\/protocol-auth\/([^/]+)\/cancel$/, cancelProtocolAuthFlow],
  ['GET', /^\/api\/protocol-auth\/([^/]+)\/elicitations$/, listProtocolElicitations],
  ['POST', /^\/api\/protocol-auth\/([^/]+)\/elicitations\/([^/]+)\/respond$/, respondProtocolElicitation],
  ['GET', /^\/api\/protocol-auth\/([^/]+)\/interaction$/, getProtocolAuthInteraction],
  ['POST', /^\/api\/protocol-auth\/([^/]+)\/interaction\/callback$/, relayProtocolAuthCallback],
  ['GET', /^\/api\/agents\/([^/]+)$/, getAgentDetail],
  ['PATCH', /^\/api\/agents\/([^/]+)$/, editAgent],
  ['DELETE', /^\/api\/agents\/([^/]+)$/, removeAgent],
  ['GET', /^\/api\/status$/, getStatus],
];

const server = createServer(async (request, response) => {
  const url = new URL(request.url, 'http://localhost');
  const method = request.method ?? 'GET';

  // The Angular dev server proxies the API, so a browser preflight is rare.
  // It is answered anyway, to keep a direct browser call working.
  if (method === 'OPTIONS') {
    response.writeHead(204, corsHeaders()).end();
    return;
  }

  for (const [routeMethod, pattern, handler] of routes) {
    if (routeMethod !== method) continue;
    const match = pattern.exec(url.pathname);
    if (!match) continue;

    try {
      const body = await readJsonBody(request);
      const params = match.slice(1).map(decodeURIComponent);
      const result = await handler({ params, body, url });
      send(response, result);
    } catch (error) {
      const status = error.status ?? 500;
      log(`${method} ${url.pathname} -> ${status} ${error.message}`);
      const errBody = { error: error.message };
      if (error.code) errBody.code = error.code;
      if (error.details) errBody.details = error.details;
      send(response, { status, value: errBody });
    }
    return;
  }

  send(response, { status: 404, value: { error: 'Not found' } });
});

server.on('upgrade', (request, socket, head) => {
  const url = new URL(request.url, 'http://localhost');
  const flowMatch = /^\/api\/agent-auth\/([^/]+)\/ws$/.exec(url.pathname);
  if (flowMatch) {
    handleAuthFlowSocket(request, socket, head, decodeURIComponent(flowMatch[1]));
    return;
  }
  if (url.pathname !== '/ws') {
    socket.end('HTTP/1.1 404 Not Found\r\n\r\n');
    return;
  }
  handleWebSocket(request, socket, head);
});

server.listen(options.port, '127.0.0.1', () => {
  log(`fake Batey backend on http://127.0.0.1:${options.port}`);
  log(`${state.projects.size} projects, ${state.chats.size} chats, latency x${options.latency}`);
  log('prompt keywords: plan, tool, permission, error, long, rich, terminal/task, quiet');
});

// ------------------------------------------------------------- websocket

function handleWebSocket(request, rawSocket, head) {
  const socket = upgrade(request, rawSocket, head);
  if (!socket) return;

  let unsubscribe = null;

  socket.onMessage = (text) => {
    let message;
    try {
      message = JSON.parse(text);
    } catch {
      return; // A malformed frame cannot change state.
    }

    if (message.type === 'subscribe') {
      const fromSeq = Number(message.from_seq) || 0;
      // A fresh browser gets only the live baseline. Reconnects use the last
      // durable sequence and replay the missed global interval.
      if (fromSeq > 0) {
        for (const event of state.replayFrom(fromSeq)) {
          socket.send(JSON.stringify(event));
        }
      }
      socket.send(JSON.stringify({ type: 'subscribed', through_seq: state.nextSeq - 1 }));

      // Live events start only after the replay, so no event is sent twice.
      unsubscribe?.();
      let lastSent = state.nextSeq - 1;
      unsubscribe = state.subscribe((event) => {
        if (event.seq <= lastSent) return;
        lastSent = event.seq;
        socket.send(JSON.stringify(event));
      });
      return;
    }

    if (message.type === 'permission_response') {
      answerPermission(message.session_id, message.id, message.option_id);
    }
    // Prompt and cancel over the WebSocket are unsupported, as in the backend.
  };

  socket.onClose = () => unsubscribe?.();
}

/** Opens the simulated PTY WebSocket for one opaque authentication flow. */
function handleAuthFlowSocket(request, rawSocket, head, flowId) {
  if (!state.flowView(flowId)) {
    rawSocket.end('HTTP/1.1 404 Not Found\r\n\r\n');
    return;
  }
  const socket = upgrade(request, rawSocket, head);
  if (!socket) return;
  state.attachFlowSocket(flowId, socket);
}

// --------------------------------------------------------------- handlers

function createProject({ body }) {
  const name = requireString(body, 'name');
  const path = requireString(body, 'path');
  requireInsideRoots(path);
  if ([...state.projects.values()].some((project) => project.path === path)) {
    throw httpError(409, 'A project already uses that directory');
  }
  const project = state.createProject(name, path);
  state.metadataChanged();
  return json(state.projectView(project), 200);
}

function editProject({ params, body }) {
  const project = state.projects.get(params[0]);
  if (!project) throw httpError(404, 'Project not found');

  const name = requireString(body, 'name');
  const path = requireString(body, 'path');
  requireInsideRoots(path);

  const hasChats = [...state.chats.values()].some((chat) => chat.project_id === project.id);
  if (project.path !== path && hasChats) {
    throw httpError(
      409,
      "Move or delete the project's chats before changing its path; saved ACP sessions belong to their original directory",
    );
  }

  project.name = name;
  project.path = path;
  project.updated_at = new Date().toISOString();
  state.metadataChanged();
  return json(state.projectView(project));
}

/**
 * Deletes a project and every chat it owns as one cascading operation,
 * mirroring the real backend's `HubService::delete_project`. Project files
 * are a fake-backend fiction (there is no real filesystem here), so nothing
 * beyond in-memory state is ever touched.
 */
function deleteProject({ params }) {
  const project = state.projects.get(params[0]);
  if (!project) throw httpError(404, 'Project not found');

  const ownedChatIds = new Set(
    [...state.chats.values()].filter((chat) => chat.project_id === project.id).map((chat) => chat.id),
  );
  const referencedExternally = [...state.additionalRootsByChat.entries()].some(
    ([chatId, roots]) => !ownedChatIds.has(chatId) && roots.some((root) => root.project_id === project.id),
  );
  if (referencedExternally) {
    throw httpError(
      409,
      "Remove this project from every other chat's additional workspace roots before deleting it",
    );
  }

  for (const chatId of ownedChatIds) {
    deleteChatById(chatId);
  }
  state.projects.delete(project.id);
  state.metadataChanged();
  return json({ success: true });
}

async function cloneProject({ body }) {
  const url = requireString(body, 'url');
  const parentPath = requireString(body, 'parent_path');
  requireInsideRoots(parentPath);
  validateGitUrl(url);

  const name = (body.name ?? '').trim() || deriveRepoName(url);
  if (!name) throw httpError(400, 'Could not derive project name from repository URL');
  if (name.includes('/') || name.includes('\\')) {
    throw httpError(400, 'Clone destination name cannot contain path separators');
  }

  const destination = `${parentPath}/${name}`;
  if ([...state.projects.values()].some((project) => project.path === destination)) {
    throw httpError(409, `Destination directory already exists: ${destination}`);
  }

  // A real clone takes time. The delay keeps the progress state visible.
  await new Promise((resolve) => setTimeout(resolve, 1200 * options.latency));

  const project = state.createProject(name, destination);
  state.metadataChanged();
  return json(state.projectView(project));
}

function listChats({ params }) {
  if (!state.projects.has(params[0])) throw httpError(404, 'Project not found');
  return json(state.listChats(params[0]));
}

function workspaceOptions({ params }) {
  const options = state.workspaceOptions(params[0]);
  if (!options) throw httpError(404, 'Project not found');
  return json(options);
}

function createChat({ params, body }) {
  if (!state.projects.has(params[0])) throw httpError(404, 'Project not found');
  const agent = requireString(body, 'agent');
  if (!AGENTS.some((candidate) => candidate.id === agent)) throw httpError(400, 'Unknown agent');

  const workspace = body.workspace;
  if (workspace !== undefined) {
    if (!workspace || !['managed_worktree', 'project_checkout'].includes(workspace.mode)) {
      throw httpError(400, 'Unknown workspace mode');
    }
    const options = state.workspaceOptions(params[0]);
    if (!options?.is_git) throw httpError(400, 'Workspace selection is only available for Git projects');
    if (typeof workspace.branch !== 'string' || !options.branches.some((branch) => branch.name === workspace.branch)) {
      throw httpError(400, 'Branch is not a local branch');
    }
  }

  const chat = state.createChat(params[0], agent, body.title, workspace);
  state.metadataChanged();
  return json(state.chatView(chat));
}

function validateAgent({ body }) { return json(validateCustomInput(body)); }

function createAgent({ body }) {
  const report = validateCustomInput(body);
  if (!report.valid) throw httpError(400, report.issues.map((issue) => `${issue.field}: ${issue.message}`).join('; '));
  return json(state.createCustomAgent(body), 200);
}

function editAgent({ params, body }) {
  const report = validateCustomInput(body);
  if (!report.valid) throw httpError(400, report.issues.map((issue) => `${issue.field}: ${issue.message}`).join('; '));
  return json(state.editCustomAgent(params[0], body));
}

function removeAgent({ params }) { return json(state.removeAgent(params[0])); }

function getAgentDetail({ params }) { return json(state.agentDetail(params[0])); }

function getAgentEnv({ params }) { return json(state.agentEnvPresence(params[0])); }

function updateAgentEnv({ params, body }) {
  const edits = Array.isArray(body) ? body : body?.edits ?? [];
  return json(state.applyAgentEnvEdits(params[0], edits));
}

function registryAgents({ url }) {
  return json(state.registryView(url.searchParams.get('q') ?? ''));
}

function refreshRegistry() { return json(state.registryView('', true)); }

function installRegistryAgent({ body }) { return json(state.startInstall(body, { autoAdvance: true })); }

function updateRegistryAgent({ params }) { return json(state.startUpdate(params[0], { autoAdvance: true })); }

function listAgentOperations() { return json(state.listAgentOperations()); }

function getAgentOperation({ params }) {
  const op = state.getAgentOperation(params[0]);
  if (!op) return json({ message: `Operation '${params[0]}' not found` }, 404);
  return json(op);
}

// ------------------------------------------------------- authentication

function getAgentAuth({ params }) { return json(state.agentAuth(params[0])); }

/** Explicit refresh: the only read path that stamps `checked_at` here. */
function refreshAgentAuth({ params }) { return json(state.refreshAgentAuth(params[0])); }

function authenticateAgentRoute({ params }) {
  return json(state.authenticateAgent(params[0], params[1]));
}

function logoutAgentRoute({ params }) { return json(state.logoutAgent(params[0])); }

function startTerminalAuth({ params }) {
  const flow = state.startTerminalFlow(params[0], params[1]);
  return json(state.flowView(flow.flow_id), 200);
}

function getAgentAuthFlow({ params }) {
  const flow = state.flowView(params[0]);
  if (!flow) throw httpError(404, 'Authentication flow not found');
  return json(flow);
}

function cancelAgentAuthFlow({ params }) { return json(state.cancelFlow(params[0])); }

function startProtocolAuth({ params }) {
  return json(state.startProtocolFlow(params[0], params[1]), 200);
}

function getProtocolAuthFlow({ params }) {
  const flow = state.protocolFlowView(params[0]);
  if (!flow) throw httpError(404, 'Authentication flow not found');
  return json(flow);
}

function cancelProtocolAuthFlow({ params }) { return json(state.cancelProtocolFlow(params[0])); }

function listProtocolElicitations({ params }) {
  return json(state.listProtocolElicitations(params[0]));
}

function respondProtocolElicitation({ params, body }) {
  const action = typeof body.action === 'string' ? body.action : '';
  if (!['accept', 'decline', 'cancel'].includes(action)) {
    throw httpError(400, 'Elicitation action must be accept, decline, or cancel');
  }
  state.respondProtocolElicitation(params[0], params[1], action, body.content ?? null);
  return json({ success: true });
}

function getProtocolAuthInteraction({ params }) {
  return json(state.protocolAuthInteraction(params[0]));
}

function relayProtocolAuthCallback({ params, body }) {
  const input = parseJson(body);
  state.relayProtocolAuthCallback(params[0], input.callback_url);
  return json({ success: true });
}

function getChat({ params }) {
  return json(state.chatView(requireChat(params[0])));
}

async function history({ params, url }) {
  const chat = requireChat(params[0]);
  if (options.historyDelayMs > 0) {
    await new Promise((resolve) => setTimeout(resolve, options.historyDelayMs * options.latency));
  }
  if (options.failHistoryPages > 0) {
    options.failHistoryPages -= 1;
    throw httpError(503, 'History page temporarily unavailable');
  }
  const before = url.searchParams.has('before_seq')
    ? Number(url.searchParams.get('before_seq'))
    : undefined;
  const through = url.searchParams.has('through_seq')
    ? Number(url.searchParams.get('through_seq'))
    : undefined;
  const requested = url.searchParams.has('limit') ? Number(url.searchParams.get('limit')) : 100;
  const limit = Number.isFinite(requested) ? Math.min(Math.max(Math.trunc(requested), 1), 200) : 100;
  return json(state.historyPage(chat.id, before, limit, through));
}

function editChat({ params, body }) {
  const chat = requireChat(params[0]);

  // Validate the whole patch and reject guarded mutations before changing
  // title or any other field, matching the Rust service's atomic compound
  // edit behavior.
  const title = body.title !== undefined ? requireString(body, 'title') : undefined;
  if (title !== undefined && title.length > 200) {
    throw httpError(400, 'Name must contain 1–200 bytes');
  }
  const archived = body.archived !== undefined ? Boolean(body.archived) : undefined;
  if (archived !== undefined && isRunning(chat.id)) {
    throw httpError(409, 'Wait for or cancel the active turn before editing the chat');
  }

  if (title !== undefined) {
    chat.title = title;
    chat.title_overridden = true;
  }
  if (archived !== undefined) {
    chat.archived = archived;
  }

  chat.updated_at = new Date().toISOString();
  state.metadataChanged();
  return json(state.chatView(chat));
}

/** Shared cleanup for one chat: durable metadata, runtime state, and events. */
function deleteChatById(chatId) {
  cancel(chatId);
  state.chats.delete(chatId);
  state.configByChat.delete(chatId);
  state.runtime.delete(chatId);
  state.forgetChat(chatId);
}

function deleteChat({ params }) {
  const chat = requireChat(params[0]);
  deleteChatById(chat.id);
  state.metadataChanged();
  return json({ success: true });
}

function getCommands({ params }) {
  const chat = requireChat(params[0]);
  ensureRunning(chat);
  return json(state.commandsByChat.get(chat.id) ?? []);
}

function getModes({ params }) {
  const chat = requireChat(params[0]);
  ensureRunning(chat);
  return json(state.modesByChat.get(chat.id) ?? null);
}

function setMode({ params, body }) {
  const chat = requireChat(params[0]);
  if (isRunning(chat.id)) throw httpError(409, 'Wait for the active turn before changing mode');
  const modes = state.modesByChat.get(chat.id);
  if (!modes || !Array.isArray(modes.available_modes) || modes.available_modes.length === 0) {
    throw httpError(400, 'Agent does not advertise session modes');
  }
  const modeId = typeof body.mode_id === 'string' ? body.mode_id.trim() : '';
  if (!modes.available_modes.some((m) => m.id === modeId)) {
    throw httpError(400, `Unknown mode: ${modeId}`);
  }
  modes.current_mode_id = modeId;
  state.emit(chat.id, chat.agent, { type: 'session_modes', state: modes });
  return json(modes);
}

function getUsage({ params }) {
  const chat = requireChat(params[0]);
  return json(state.usageByChat.get(chat.id) ?? null);
}

function getSessionInfo({ params }) {
  const chat = requireChat(params[0]);
  return json({
    agent_info: { name: chat.agent, version: '1.0.0', title: chat.agent },
    auth_methods: [],
  });
}

function listElicitations({ params }) {
  const chat = requireChat(params[0]);
  const pending = listPendingElicitations(chat.id);
  const stored = state.elicitationsByChat.get(chat.id) ?? [];
  // Prefer live pending from turns; fall back to stored list for reconnect.
  return json(pending.length > 0 ? pending : stored);
}

function respondElicitation({ params, body }) {
  const chat = requireChat(params[0]);
  const eid = params[1];
  const action = typeof body.action === 'string' ? body.action : '';
  if (!['accept', 'decline', 'cancel'].includes(action)) {
    throw httpError(400, 'Elicitation action must be accept, decline, or cancel');
  }
  const content = body.content;
  // URL mode never carries secrets in the log; form values stay transient.
  const ok = answerElicitation(chat.id, eid, action, content ?? null);
  if (!ok) {
    // Also try stored list for idempotency after reconnect.
    const stored = state.elicitationsByChat.get(chat.id) ?? [];
    const idx = stored.findIndex((e) => e.id === eid);
    if (idx < 0) throw httpError(404, 'Elicitation not found');
    stored.splice(idx, 1);
    state.emit(chat.id, chat.agent, { type: 'elicitation_response', id: eid, action });
    return json({ success: true });
  }
  const stored = state.elicitationsByChat.get(chat.id) ?? [];
  const idx = stored.findIndex((e) => e.id === eid);
  if (idx >= 0) stored.splice(idx, 1);
  // The turn itself emits elicitation_response/complete; this endpoint only
  // acknowledges acceptance for the HTTP caller.
  return json({ success: true });
}

function deleteRemoteSession({ params }) {
  const chat = requireChat(params[0]);
  const remoteId = params[1];
  const list = state.remoteSessionsByChat.get(chat.id) ?? [];
  const idx = list.findIndex((s) => s.sessionId === remoteId);
  if (idx < 0) throw httpError(404, 'Remote session not found');
  list.splice(idx, 1);
  return json({ success: true });
}

function promptChat({ params, body }) {
  const chat = requireChat(params[0]);
  if (state.isEnvironmentBlocked(chat.id)) {
    throw envrcBlockedError();
  }
  const text = typeof body.text === 'string' ? body.text : '';
  const content = Array.isArray(body.content) ? body.content : null;
  if ((content && typeof body.text === 'string') || (!content && (!text.trim() || text.length > 100_000))) {
    throw httpError(400, 'Prompt must contain 1–100000 bytes');
  }
  if (content && !validRichContent(content)) throw httpError(400, 'Unsupported or oversized rich prompt content');
  if (isRunning(chat.id)) {
    throw httpError(409, 'Wait for the active turn to finish');
  }

  ensureRunning(chat);
  startTurn(state, chat, content ? content.map((block) => block.type === 'text' ? block.text : '').join('\n') : text, options.latency, content ?? undefined);
  // The backend answers 202 and streams the result on the WebSocket.
  return json({ accepted: true }, 202);
}

function validRichContent(content) {
  if (!content.length) return false;
  let total = 0;
  for (const block of content) {
    if (!block || typeof block.type !== 'string') return false;
    if (block.type === 'text' && typeof block.text === 'string') { total += block.text.length; continue; }
    if ((block.type === 'image' || block.type === 'audio') && typeof block.data === 'string' && typeof block.mimeType === 'string') {
      const allowed = block.type === 'image' ? ['image/png', 'image/jpeg', 'image/gif', 'image/webp'] : ['audio/mpeg', 'audio/wav', 'audio/ogg', 'audio/webm'];
      if (!allowed.includes(block.mimeType) || !/^[A-Za-z0-9+/]*={0,2}$/.test(block.data)) return false;
      const bytes = Buffer.from(block.data, 'base64');
      const imageOk = block.mimeType === 'image/png' ? bytes.subarray(0, 8).equals(Buffer.from([137,80,78,71,13,10,26,10]))
        : block.mimeType === 'image/jpeg' ? bytes.subarray(0, 3).equals(Buffer.from([255,216,255]))
        : block.mimeType === 'image/gif' ? bytes.subarray(0, 3).toString() === 'GIF'
        : block.mimeType === 'image/webp' ? bytes.subarray(0, 4).toString() === 'RIFF' && bytes.subarray(8, 12).toString() === 'WEBP'
        : block.mimeType === 'audio/mpeg' ? bytes.subarray(0, 3).toString() === 'ID3' || bytes[0] === 255
        : block.mimeType === 'audio/wav' ? bytes.subarray(0, 4).toString() === 'RIFF' && bytes.subarray(8, 12).toString() === 'WAVE'
        : block.mimeType === 'audio/ogg' ? bytes.subarray(0, 4).toString() === 'OggS'
        : bytes.subarray(0, 4).equals(Buffer.from([0x1a, 0x45, 0xdf, 0xa3]));
      if (!imageOk) return false;
      total += bytes.length; if (total > 4 * 1024 * 1024 || bytes.length > 2 * 1024 * 1024) return false; continue;
    }
    if (block.type === 'resource' && block.resource && typeof block.resource.uri === 'string' && (typeof block.resource.text === 'string' || typeof block.resource.blob === 'string')) {
      const bytes = typeof block.resource.text === 'string' ? Buffer.byteLength(block.resource.text) : Buffer.from(block.resource.blob, 'base64').length;
      total += bytes; if (bytes > 512 * 1024) return false; continue;
    }
    if (block.type === 'resource_link' && typeof block.uri === 'string' && /^https?:\/\//i.test(block.uri)) { total += block.uri.length; continue; }
    return false;
  }
  return total <= 4 * 1024 * 1024;
}

function cancelChat({ params }) {
  const chat = requireChat(params[0]);
  if (cancel(chat.id)) state.setRuntime(chat.id, 'RUNNING', 'CANCELLING');
  return json({ success: true });
}

async function resumeChat({ params }) {
  const chat = requireChat(params[0]);
  if (chat.archived) throw httpError(409, 'Restore the chat before you connect it');
  if (state.isEnvironmentBlocked(chat.id)) {
    throw envrcBlockedError();
  }

  state.setRuntime(chat.id, 'STARTING', 'IDLE');
  if (!chat.acp_session_id) chat.acp_session_id = `acp-${randomUUID()}`;

  // `AcpSession::resume` awaits the process, so the real endpoint answers only
  // after the agent runs. The caller fetches the config right after this
  // response, and that fetch must not race the spawn.
  await new Promise((resolve) => setTimeout(resolve, 400 * options.latency));

  state.setRuntime(chat.id, 'RUNNING', 'IDLE');
  state.emit(chat.id, chat.agent, {
    type: 'config_options',
    options: state.configByChat.get(chat.id) ?? defaultConfigOptions(chat.agent),
  });
  const modes = state.modesByChat.get(chat.id);
  if (modes) state.emit(chat.id, chat.agent, { type: 'session_modes', state: modes });
  const commands = state.commandsByChat.get(chat.id);
  if (commands?.length) state.emit(chat.id, chat.agent, { type: 'available_commands', commands });

  return json(state.chatView(chat));
}

function stopChat({ params }) {
  const chat = requireChat(params[0]);
  cancel(chat.id);
  state.setRuntime(chat.id, 'STOPPED', 'IDLE');
  return json({ success: true });
}

function respondPermission({ params, body }) {
  const chat = requireChat(params[0]);
  const id = requireString(body, 'id');
  const optionId = requireString(body, 'option_id');
  if (!answerPermission(chat.id, id, optionId)) {
    throw httpError(409, 'Permission request is stale or the option is not advertised by the agent');
  }
  return json({ success: true });
}

function getConfig({ params }) {
  const chat = requireChat(params[0]);
  ensureRunning(chat);
  return json(state.configByChat.get(chat.id) ?? []);
}

function setConfig({ params, body }) {
  const chat = requireChat(params[0]);
  if (isRunning(chat.id)) {
    throw httpError(400, 'Wait for the active turn before changing configuration');
  }

  const id = requireString(body, 'id');
  const config = state.configByChat.get(chat.id) ?? [];
  const option = config.find((entry) => entry.id === id);
  if (!option) throw httpError(400, `Unknown config option: ${id}`);

  // Stable boolean options require a boolean value; select options must pick
  // from advertised values. Preserve ordering/descriptions generically.
  if (option.type === 'boolean' && typeof body.value !== 'boolean') {
    throw httpError(400, 'Unsupported ACP config value');
  }
  if (option.type === 'select') {
    const flat = [];
    for (const entry of option.options ?? []) {
      if (entry.options) flat.push(...entry.options);
      else flat.push(entry);
    }
    if (!flat.some((v) => v.value === body.value)) {
      throw httpError(400, 'Unsupported ACP config value');
    }
  }

  option.currentValue = body.value;
  chat.config_values = { ...chat.config_values, [id]: body.value };

  state.emit(chat.id, chat.agent, { type: 'config_options', options: config });
  return json(config);
}

function clearConfig({ params }) {
  const chat = requireChat(params[0]);
  const optionId = params[1];
  const next = { ...chat.config_values };
  delete next[optionId];
  chat.config_values = next;
  return { status: 204, value: null };
}

function mcpView(server) {
  return { ...server, secrets: (server.secrets ?? []).map(({ name, value }) => ({ name, present: Boolean(value) })) };
}
function validateMcp(body) {
  const transports = ['stdio', 'http', 'sse'];
  if (!body || !transports.includes(body.transport) || typeof body.name !== 'string' || !body.name.trim()) throw httpError(400, 'Unsupported MCP transport or missing name');
  if (['http', 'sse'].includes(body.transport)) {
    if (typeof body.url !== 'string' || !/^https?:\/\//.test(body.url) || /^https?:\/\/[^/]*@/.test(body.url) || body.command != null) throw httpError(400, 'MCP URL must be http(s) without userinfo');
  } else if (typeof body.command !== 'string' || !body.command.startsWith('/') || body.url != null) throw httpError(400, 'Stdio MCP command must be absolute');
}
function requireIdleConnectionEdit(chat) { if (isRunning(chat.id)) throw httpError(409, 'Wait for the active turn before changing connection configuration'); state.setRuntime(chat.id, 'STOPPED', 'IDLE'); }
function getMcpServers({ params }) { requireChat(params[0]); return json((state.mcpByChat.get(params[0]) ?? []).map(mcpView)); }
function createMcpServer({ params, body }) {
  const chat = requireChat(params[0]); validateMcp(body); requireIdleConnectionEdit(chat);
  const secrets = (body.secrets ?? []).map((s) => ({ name: String(s.name ?? ''), value: s.value ?? '' }));
  if (secrets.some((s) => !s.name)) throw httpError(400, 'MCP secret name is invalid');
  const values = state.mcpByChat.get(chat.id) ?? []; values.push({ id: randomUUID(), position: values.length, name: body.name.trim(), transport: body.transport, url: body.url ?? null, command: body.command ?? null, args: body.args ?? [], secrets }); state.mcpByChat.set(chat.id, values); state.metadataChanged(); return json(values.map(mcpView));
}
function editMcpServer({ params, body }) {
  const chat = requireChat(params[0]); validateMcp(body); requireIdleConnectionEdit(chat); const values = state.mcpByChat.get(chat.id) ?? []; const value = values.find((s) => s.id === params[1]); if (!value) throw httpError(404, 'MCP server not found');
  const secrets = [...(value.secrets ?? [])]; for (const edit of body.secrets ?? []) { const action = edit.action ?? 'replace'; const index = secrets.findIndex((s) => s.name === edit.name); if (action === 'keep') { if (index < 0) throw httpError(400, 'Cannot keep an unknown MCP secret'); } else if (action === 'remove') { if (index >= 0) secrets.splice(index, 1); } else { if (typeof edit.value !== 'string' || !edit.value) throw httpError(400, 'A replacement MCP secret value is required'); if (index >= 0) secrets.splice(index, 1); secrets.push({ name: edit.name, value: edit.value }); } }
  Object.assign(value, { name: body.name.trim(), transport: body.transport, url: body.url ?? null, command: body.command ?? null, args: body.args ?? [], secrets }); state.metadataChanged(); return json(values.map(mcpView));
}
function deleteMcpServer({ params }) { const chat = requireChat(params[0]); requireIdleConnectionEdit(chat); const values = state.mcpByChat.get(chat.id) ?? []; const index = values.findIndex((s) => s.id === params[1]); if (index < 0) throw httpError(404, 'MCP server not found'); values.splice(index, 1); values.forEach((s, position) => { s.position = position; }); state.metadataChanged(); return { status: 204, value: null }; }
function orderMcpServers({ params, body }) { const chat = requireChat(params[0]); requireIdleConnectionEdit(chat); const values = state.mcpByChat.get(chat.id) ?? []; if (!Array.isArray(body.ids) || body.ids.length !== values.length || new Set(body.ids).size !== values.length) throw httpError(400, 'MCP order must contain every server exactly once'); const ordered = body.ids.map((id, position) => { const value = values.find((s) => s.id === id); if (!value) throw httpError(400, 'MCP order must contain every server exactly once'); return { ...value, position }; }); state.mcpByChat.set(chat.id, ordered); state.metadataChanged(); return json(ordered.map(mcpView)); }
function getAdditionalRoots({ params }) { requireChat(params[0]); return json(state.additionalRootsByChat.get(params[0]) ?? []); }
function setAdditionalRoots({ params, body }) { const chat = requireChat(params[0]); requireIdleConnectionEdit(chat); if (!Array.isArray(body.project_ids) || new Set(body.project_ids).size !== body.project_ids.length) throw httpError(400, 'Additional projects must be unique'); const roots = body.project_ids.map((projectId, position) => { if (!state.projects.has(projectId)) throw httpError(404, 'Project not found'); return { project_id: projectId, position }; }); state.additionalRootsByChat.set(chat.id, roots); state.metadataChanged(); return json(roots); }

function remoteSessions({ params }) {
  const chat = requireChat(params[0]);
  const stored = state.remoteSessionsByChat.get(chat.id);
  if (stored) {
    // Keep the live ACP id first so the UI can match the current session.
    const liveId = chat.acp_session_id;
    const sessions = liveId && !stored.some((s) => s.sessionId === liveId)
      ? [{ sessionId: liveId, title: chat.title }, ...stored]
      : stored;
    return json({ sessions, nextCursor: null });
  }
  return json({
    sessions: [
      { sessionId: chat.acp_session_id ?? 'acp-unknown', title: chat.title },
      { sessionId: 'acp-older-session', title: 'Earlier conversation' },
    ],
    nextCursor: null,
  });
}

function getStatus() {
  return json({
    project_root: PROJECT_ROOT,
    agents: AGENTS.map(({ id }) => ({
      name: id,
      process_state: 'STOPPED',
      turn_state: 'IDLE',
    })),
  });
}

// ------------------------------------------------------- fake filesystem

// A synthetic tree. It keeps the folder picker independent of the machine.
const DIRECTORY_TREE = {
  [PROJECT_ROOT]: ['batey', 'corolla-firmware', 'scratch', 'vendor'],
  [`${PROJECT_ROOT}/batey`]: ['frontend', 'src', 'tests'],
  [`${PROJECT_ROOT}/batey/frontend`]: ['public', 'src'],
  [`${PROJECT_ROOT}/batey/src`]: ['acp', 'config', 'session', 'state', 'web'],
  [`${PROJECT_ROOT}/corolla-firmware`]: ['calibration', 'flash', 'tools'],
  [`${PROJECT_ROOT}/scratch`]: [],
  [`${PROJECT_ROOT}/vendor`]: ['libmvci', 'openssl'],
};

function listDirectories({ url }) {
  const requested = url.searchParams.get('path');
  const target = requested && requested.trim() ? requested : PROJECT_ROOT;

  if (!target.startsWith('/')) throw httpError(400, 'Directory path must be absolute');
  if (!target.startsWith(PROJECT_ROOT)) {
    throw httpError(403, 'Directory is outside configured project roots');
  }
  const children = DIRECTORY_TREE[target];
  if (!children) throw httpError(404, 'Directory does not exist');

  const segments = target.slice(PROJECT_ROOT.length).split('/').filter(Boolean);

  const breadcrumbs = [{ name: 'projects', path: PROJECT_ROOT }];
  let walked = PROJECT_ROOT;
  for (const segment of segments) {
    walked = `${walked}/${segment}`;
    breadcrumbs.push({ name: segment, path: walked });
  }

  return json({
    current: target,
    name: segments.at(-1) ?? 'projects',
    parent: target === PROJECT_ROOT ? null : target.slice(0, target.lastIndexOf('/')),
    roots: [PROJECT_ROOT],
    breadcrumbs,
    directories: children.map((name) => ({ name, path: `${target}/${name}` })),
  });
}

// --------------------------------------------------------------- helpers

function ensureRunning(chat) {
  if (state.isEnvironmentBlocked(chat.id)) {
    throw envrcBlockedError();
  }
  const runtime = state.runtime.get(chat.id);
  if (runtime?.process !== 'RUNNING') {
    if (!chat.acp_session_id) chat.acp_session_id = `acp-${randomUUID()}`;
    state.setRuntime(chat.id, 'RUNNING', 'IDLE');
  }
}

function requireChat(id) {
  const chat = state.chats.get(id);
  if (!chat) throw httpError(404, 'Chat not found');
  return chat;
}

function requireString(body, field) {
  const value = body?.[field];
  if (typeof value !== 'string' || !value.trim()) {
    throw httpError(400, `Field "${field}" must be a non-empty string`);
  }
  return value.trim();
}

function requireInsideRoots(path) {
  if (!path.startsWith('/')) throw httpError(400, 'Path must be absolute');
  if (!path.startsWith(PROJECT_ROOT)) {
    throw httpError(403, `Path is outside the configured project root ${PROJECT_ROOT}`);
  }
}

/** Mirrors `hub::validate_git_url`. */
function validateGitUrl(url) {
  const trimmed = url.trim();
  const lower = trimmed.toLowerCase();
  if (
    trimmed.startsWith('file://') ||
    trimmed.startsWith('/') ||
    trimmed.startsWith('./') ||
    trimmed.startsWith('../') ||
    trimmed.startsWith('~') ||
    trimmed.includes('::')
  ) {
    throw httpError(400, 'Unsafe or unsupported repository URL transport');
  }
  if (lower.startsWith('http://')) {
    throw httpError(400, 'Plain HTTP repository URLs are not allowed. Use HTTPS or SSH');
  }
  const isUrl = lower.startsWith('https://') || lower.startsWith('ssh://');
  const isScp = trimmed.includes('@') && trimmed.includes(':') && !trimmed.includes('://');
  if (!isUrl && !isScp) {
    throw httpError(400, 'Repository URL must be a valid HTTPS or SSH URL');
  }
}

function deriveRepoName(url) {
  const trimmed = url.trim().replace(/\/+$/, '');
  const withoutGit = trimmed.endsWith('.git') ? trimmed.slice(0, -4) : trimmed;
  return withoutGit.split(/[/:]/).pop() ?? '';
}

function json(value, status = 200) {
  return { status, value };
}

function httpError(status, message, extra = {}) {
  const error = new Error(message);
  error.status = status;
  Object.assign(error, extra);
  return error;
}

/** Matches the real backend's structured, sanitized `envrc_blocked` shape
 * (T125): a fixed friendly message, with the raw diagnostic kept only in
 * `details.message` for debugging. */
function envrcBlockedError() {
  return httpError(409, "This project's workspace environment needs approval.", {
    code: 'envrc_blocked',
    details: {
      path: `${PROJECT_ROOT}/.envrc`,
      relative_path: '.envrc',
      message: "direnv: error .envrc is blocked. Run \"direnv allow\" to approve its content",
    },
  });
}

function corsHeaders() {
  return {
    'access-control-allow-origin': '*',
    'access-control-allow-methods': 'GET,POST,PATCH,DELETE,OPTIONS',
    'access-control-allow-headers': 'content-type,authorization',
  };
}

function send(response, { status, value }) {
  const body = JSON.stringify(value ?? null);
  response.writeHead(status, {
    'content-type': 'application/json',
    'content-length': Buffer.byteLength(body),
    ...corsHeaders(),
  });
  response.end(body);
}

function readJsonBody(request) {
  if (request.method === 'GET' || request.method === 'DELETE') return Promise.resolve({});

  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    request.on('data', (chunk) => {
      size += chunk.length;
      if (size > 1_000_000) {
        reject(httpError(413, 'Request body is too large'));
        request.destroy();
        return;
      }
      chunks.push(chunk);
    });
    request.on('end', () => {
      const raw = Buffer.concat(chunks).toString('utf8');
      if (!raw.trim()) return resolve({});
      try {
        resolve(JSON.parse(raw));
      } catch {
        reject(httpError(400, 'Request body is not valid JSON'));
      }
    });
    request.on('error', reject);
  });
}

function parseArgs(argv) {
  const parsed = { port: 8765, latency: 1, historyDelayMs: 0, failHistoryPages: 0 };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '--port') parsed.port = Number(argv[++index]);
    else if (arg === '--latency') parsed.latency = Number(argv[++index]);
    else if (arg === '--history-delay-ms') parsed.historyDelayMs = Number(argv[++index]);
    else if (arg === '--fail-history-pages') parsed.failHistoryPages = Number(argv[++index]);
    else if (arg === '--help') {
      console.log('Usage: node fake-backend/server.mjs [--port 8765] [--latency 1.0] [--history-delay-ms 0] [--fail-history-pages 0]');
      process.exit(0);
    }
  }
  if (!Number.isFinite(parsed.port) || parsed.port <= 0) throw new Error('Invalid --port');
  if (!Number.isFinite(parsed.latency) || parsed.latency < 0) throw new Error('Invalid --latency');
  if (!Number.isFinite(parsed.historyDelayMs) || parsed.historyDelayMs < 0) throw new Error('Invalid --history-delay-ms');
  if (!Number.isInteger(parsed.failHistoryPages) || parsed.failHistoryPages < 0) throw new Error('Invalid --fail-history-pages');
  return parsed;
}

function log(message) {
  console.log(`[fake-backend] ${message}`);
}

function authorizeEnvironment({ params, body }) {
  const chat = requireChat(params[0]);
  state.authorizeEnvironment(chat.id, body?.remember === true);
  return json({ success: true });
}

function forgetProjectEnvrcGrant({ params }) {
  const project = state.projects.get(params[0]);
  if (!project) throw httpError(404, 'Project not found');
  state.forgetProjectEnvrc(project.id);
  return json({ success: true });
}

function listTasks({ params }) {
  const chat = requireChat(params[0]);
  return json(state.listTasks(chat.id));
}

function getTask({ params }) {
  const chat = requireChat(params[0]);
  const task = state.getTask(chat.id, params[1]);
  if (!task) throw httpError(404, 'Task not found');
  return json(task);
}

function stopTask({ params }) {
  const chat = requireChat(params[0]);
  const stopped = state.stopTask(chat.id, params[1]);
  if (!stopped) throw httpError(404, 'Task not found');
  return json({ success: true });
}
