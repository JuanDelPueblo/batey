export type ProcessState = 'STARTING' | 'RUNNING' | 'STOPPED' | 'DEAD';
export type TurnState = 'IDLE' | 'PROMPTING' | 'CANCELLING';
/** Legacy agent/database value. It is accepted for compatibility but never
 * controls ACP callbacks. */
export type PermissionPolicy = 'ask' | 'read-only' | 'auto-approve' | 'deny-all';
export type AgentSource = 'builtin' | 'file' | 'batey_managed' | 'registry' | 'declarative';
export type AgentAvailability = 'available' | 'unavailable';
/** Who may change the definition. Registry entries use their own lifecycle. */
export type AgentMutability = 'editable' | 'registry_managed' | 'read_only';

/** Provider-neutral presentation metadata. Never used to start a process. */
export interface AgentDisplay {
  description?: string | null;
  version?: string | null;
  icon?: string | null;
  repository?: string | null;
  website?: string | null;
  license?: string | null;
  license_url?: string | null;
  authors?: string[];
}

export interface AgentSummary {
  id: string;
  display_name: string;
  source: AgentSource;
  availability: AgentAvailability;
  usage_provider?: string | null;
  metadata: unknown;
  mutability?: AgentMutability;
  display?: AgentDisplay;
  unavailable_reason?: string | null;
}

/** Authenticated management data for an editable Batey-managed definition. */
export interface AgentManagementDetail {
  id: string;
  display_name: string;
  command: string;
  args: string[];
  env: Record<string, string>;
  idle_timeout: number;
  usage_provider?: string | null;
  metadata: unknown;
  default_permission_policy: PermissionPolicy;
  description?: string | null;
}

/** What a management surface sends to create or edit a custom definition. */
export interface CustomAgentInput {
  id: string;
  display_name?: string | null;
  command: string;
  args: string[];
  env: Record<string, string>;
  idle_timeout?: number | null;
  usage_provider?: string | null;
  metadata?: unknown;
  default_permission_policy?: PermissionPolicy | null;
  description?: string | null;
}

export interface ValidationIssue {
  field: string;
  message: string;
}

export interface ValidationReport {
  valid: boolean;
  issues: ValidationIssue[];
}

export type RegistryStatus = 'fresh' | 'cached' | 'unavailable';
export type DistributionKind = 'binary' | 'npx' | 'uvx';
export type PlatformTarget =
  | 'darwin-aarch64'
  | 'darwin-x86_64'
  | 'linux-aarch64'
  | 'linux-x86_64'
  | 'windows-aarch64'
  | 'windows-x86_64';

export interface RegistryEntry {
  id: string;
  name: string;
  version: string;
  description: string;
  repository?: string | null;
  website?: string | null;
  authors?: string[];
  license?: string | null;
  license_url?: string | null;
  icon?: string | null;
  distributions: DistributionKind[];
  platforms: PlatformTarget[];
  selected_distribution?: DistributionKind | null;
  unsupported_reason?: string | null;
  installed_as?: string | null;
  installed_version?: string | null;
  update_available: boolean;
}

export interface RegistryCatalog {
  status: RegistryStatus;
  source_url: string;
  registry_version?: string | null;
  fetched_at?: string | null;
  error?: string | null;
  host_platform?: PlatformTarget | null;
  host: string;
  rejected?: Array<{ id?: string | null; reason?: string } & Record<string, unknown>>;
  agents: RegistryEntry[];
}

export type AgentOperationKind = 'install' | 'update';

export type AgentOperationState = 'running' | 'succeeded' | 'failed';

export type AgentOperationStage =
  | 'queued'
  | 'resolving'
  | 'downloading'
  | 'verifying'
  | 'extracting'
  | 'preparing'
  | 'finalizing'
  | 'completed'
  | 'failed';

export interface AgentOperation {
  id: string;
  kind: AgentOperationKind;
  agent_id: string;
  registry_id: string;
  state: AgentOperationState;
  stage: AgentOperationStage;
  bytes_downloaded: number;
  total_bytes?: number | null;
  error?: string | null;
  update_outcome?: UpdateOutcome | null;
  created_at: string;
  updated_at: string;
}

export interface InstallRegistryAgentInput {
  registry_id: string;
  agent_id?: string;
  distribution?: DistributionKind;
  display_name?: string;
  usage_provider?: string;
  idle_timeout?: number;
  default_permission_policy?: PermissionPolicy;
  metadata?: unknown;
}

export interface UpdateOutcome {
  updated: boolean;
  from_version?: string;
  to_version?: string;
  agent?: AgentSummary | null;
  previous_install_dir?: string | null;
  operation?: AgentOperation;
}

export interface AgentEnvPresence { name: string; present: boolean; }
export type AgentEnvAction = 'keep' | 'replace' | 'remove';
export interface AgentEnvEdit { name: string; value?: string; action?: AgentEnvAction; }

export interface RemoveOutcome {
  id: string;
  deleted: boolean;
  retained_chats: number;
  agent?: AgentSummary | null;
}

/**
 * Authentication method advertised by an ACP agent.
 * The backend carries the raw ACP type and whether this build/platform supports it.
 */
export interface AgentAuthMethod {
  id: string;
  name: string;
  type: string;
  description?: string | null;
  supported: boolean;
}

export type ObservedAuthState = 'unknown' | 'authentication_required' | 'authenticated';

/**
 * How current an `AgentAuthState` is. `unknown` means this agent was never
 * checked: there is no cache entry at all. `fresh` means this exact response
 * is the direct result of a live check that just completed. `cached` means
 * the response came from the durable cache and nothing has invalidated it.
 * `stale` means the cache is either explicitly invalidated by a mutation or
 * old enough that it should no longer be trusted without a fresh check; the
 * data is still historical evidence, never erased.
 */
export type AuthFreshness = 'unknown' | 'fresh' | 'cached' | 'stale';

/** Safe active-flow discovery. Never carries PTY output, codes, or secrets. */
export interface ActiveAuthFlow {
  flow_id: string;
  kind: 'protocol' | 'terminal';
  method_id: string;
  state: string;
  started_at?: string;
}

export interface AgentAuthState {
  agent_id: string;
  methods: AgentAuthMethod[];
  logout_supported: boolean;
  terminal_supported: boolean;
  observed_state: ObservedAuthState;
  /** Freshness of `methods`/`logout_supported` only. */
  freshness: AuthFreshness;
  /** When the methods/logout capability were last confirmed, absent when
   * never checked. */
  checked_at?: string | null;
  /**
   * Freshness of `observed_state` specifically, independent of `freshness`:
   * a bare discovery check that only reconfirms the method list never makes
   * old sign-in evidence look freshly verified, and new sign-in evidence
   * never clears a mutation's staleness on the method list. Use this field,
   * not `freshness`, to decide whether `authenticated` is current or merely
   * historical ("previously signed in").
   */
  observed_freshness: AuthFreshness;
  /** When `observed_state` was last set by real evidence, absent when
   * Batey has never observed anything for this agent. */
  observed_checked_at?: string | null;
  /** The one unfinished auth flow for this agent, when one exists. */
  active_flow?: ActiveAuthFlow | null;
}

/**
 * The response of an explicit refresh. `refresh_error` is set only when the
 * probe itself failed; `AgentAuthState` still carries the best data Batey
 * has, so a failed check never erases a useful cache or hides the agent.
 */
export interface AgentAuthRefreshResult extends AgentAuthState {
  refresh_error?: string | null;
}

export type AgentAuthFlowState =
  | 'running'
  | 'succeeded'
  | 'failed'
  | 'cancelled'
  | 'timed_out';

export type ProtocolAuthFlowState =
  | 'running'
  | 'waiting_for_user'
  | 'succeeded'
  | 'failed'
  | 'cancelled'
  | 'timed_out';

export interface ProtocolAuthFlow {
  flow_id: string;
  agent_id: string;
  method_id: string;
  state: ProtocolAuthFlowState;
  reason?: string | null;
  started_at?: string;
  completed_at?: string | null;
}

export interface ProtocolAuthElicitation {
  id: string;
  mode: string;
  message: string;
  schema?: unknown;
  url?: string | null;
  elicitation_id?: string | null;
  tool_call_id?: string | null;
}

/** Ephemeral browser interaction for one live protocol auth flow. */
export interface ProtocolAuthInteraction {
  type: 'browser';
  url: string;
  manual_callback: boolean;
}

export interface AgentAuthFlow {
  flow_id: string;
  agent_id: string;
  method_id: string;
  state: AgentAuthFlowState;
  exit_code?: number | null;
  reason?: string | null;
  started_at?: string;
  completed_at?: string | null;
}

export type AgentAuthSocketIncoming =
  | { type: 'output'; data: string }
  | {
      type: 'state';
      flow_id?: string;
      agent_id?: string;
      method_id?: string;
      state: AgentAuthFlowState;
      exit_code?: number | null;
      reason?: string | null;
    };

export type AgentAuthSocketOutgoing =
  | { type: 'input'; data: string }
  | { type: 'resize'; cols: number; rows: number };

/**
 * Structured chat failure that tells the user to authenticate. The backend
 * reports it as an `auth_required` API error code with these details.
 */
export interface AuthRequiredInfo {
  agent_id?: string;
  agent_name?: string | null;
  message?: string | null;
  methods?: AgentAuthMethod[];
}

export interface Project {
  id: string;
  name: string;
  path: string;
  created_at: string;
  updated_at: string;
  chat_count?: number;
  envrc_remembered?: boolean;
  envrc_relative_path?: string | null;
}

export type WorkspaceMode = 'managed_worktree' | 'project_checkout';

export interface WorkspaceBranch {
  name: string;
  sha: string;
  current: boolean;
}

export interface WorkspaceOptions {
  is_git: boolean;
  current_branch: string | null;
  head_sha: string | null;
  dirty: boolean;
  branches: WorkspaceBranch[];
}

export interface ChatWorkspaceSelection {
  mode: WorkspaceMode;
  branch: string;
}

export interface ChatWorkspaceSummary {
  mode: WorkspaceMode;
  branch: string | null;
  base_commit: string | null;
}

export interface Chat {
  id: string;
  project_id: string;
  agent: string;
  title: string;
  acp_session_id?: string | null;
  created_at: string;
  updated_at: string;
  archived: boolean;
  /** Legacy persisted value; ignored by the ACP runtime. */
  permission_policy?: PermissionPolicy;
  config_values: Record<string, unknown>;
  title_overridden?: boolean;
  turn_started_at?: string | null;
  process_state?: ProcessState;
  turn_state?: TurnState;
  workspace?: ChatWorkspaceSummary | null;
  active_tasks?: number;
}

export type McpTransport = 'stdio' | 'http' | 'sse';
export interface McpSecretPresence { name: string; present: boolean; }
export interface McpServer { id: string; position: number; name: string; transport: McpTransport; url: string | null; command: string | null; args: string[]; secrets: McpSecretPresence[]; }
export interface AdditionalRoot { project_id: string; position: number; }
export interface McpServerInput { name: string; transport: McpTransport; url?: string; command?: string; args?: string[]; secrets?: Array<{ name: string; value?: string; action?: 'keep' | 'replace' | 'remove' }>; }

export type TerminalTaskState = "running" | "completed" | "failed" | "stopped";

export interface TerminalTaskSummary {
  id: string;
  chat_id: string;
  command: string;
  cwd: string;
  state: TerminalTaskState;
  exit_code?: number | null;
  started_at: string;
  completed_at?: string | null;
}

export interface TerminalTaskDetails extends TerminalTaskSummary {
  output: string;
  truncated: boolean;
}

export interface BlockedEnvironmentError {
  path: string;
  relative_path?: string;
  message: string;
}

export interface ConfigOptionSelectGroup {
  group: string;
  options: ConfigOptionSelectValue[];
}

export interface ConfigOptionSelectValue {
  value: unknown;
  name: string;
  description?: string;
}

export interface ConfigOption {
  id: string;
  name: string;
  type: 'select' | 'boolean' | string;
  currentValue: unknown;
  description?: string;
  category?: string;
  options?: Array<ConfigOptionSelectValue | ConfigOptionSelectGroup>;
}

export interface AvailableCommand {
  name: string;
  description: string;
  input?: { hint: string } | null;
}

export interface SessionMode {
  id: string;
  name: string;
  description?: string | null;
}

export interface SessionModes {
  current_mode_id: string;
  available_modes: SessionMode[];
}

export interface UsageInfo {
  used: number;
  size: number;
  cost_amount?: number | null;
  cost_currency?: string | null;
}

export interface ElicitationInfo {
  id: string;
  mode: string;
  message: string;
  schema?: unknown;
  url?: string | null;
  elicitation_id?: string | null;
  tool_call_id?: string | null;
}

export interface DirectoryEntry {
  name: string;
  path: string;
}

export interface Breadcrumb {
  name: string;
  path: string;
}

export interface DirectoryListing {
  current: string;
  name: string;
  parent: string | null;
  roots: string[];
  breadcrumbs: Breadcrumb[];
  directories: DirectoryEntry[];
}

export interface CloneProjectInput {
  url: string;
  parent_path: string;
  name?: string;
}

export interface PlanEntry {
  content: string;
  status: string;
}

/** Stable ACP v1 blocks Batey accepts and renders. No executable content is a DOM surface. */
export type RichContentBlock =
  | { type: 'text'; text: string }
  | { type: 'image'; data: string; mimeType: 'image/png' | 'image/jpeg' | 'image/gif' | 'image/webp'; uri?: string }
  | { type: 'audio'; data: string; mimeType: 'audio/mpeg' | 'audio/wav' | 'audio/ogg' | 'audio/webm' }
  | { type: 'resource_link'; name: string; uri: string; title?: string; description?: string; mimeType?: string; size?: number }
  | { type: 'resource'; resource: { text: string; uri: string; mimeType?: string } | { blob: string; uri: string; mimeType?: string } };

export interface PromptContentInput { content: RichContentBlock[]; }

export interface TurnEntryMessage {
  id: number;
  type: 'message_chunk';
  text: string;
  content?: RichContentBlock[];
}

export interface TurnEntryThought {
  id: number;
  type: 'thought_chunk';
  text: string;
  content?: RichContentBlock[];
}

export interface TurnEntryTool {
  id: number;
  type: 'tool_call';
  toolCallId: string;
  title: string;
  status: string;
  output?: string | null;
  kind?: string;
  parentId?: string;
  locations?: Array<{ path: string; line?: number | null }> | null;
  content?: unknown;
}

export interface TurnEntryElicitation {
  id: number;
  type: 'elicitation_request';
  requestId: string;
  mode: string;
  message: string;
  schema?: unknown;
  url?: string | null;
  toolCallId?: string;
  responded?: boolean;
  decision?: string;
}

export interface TurnEntryPlan {
  id: number;
  type: 'plan';
  entries: PlanEntry[];
}

export interface TurnEntryPermission {
  id: number;
  type: 'permission_request';
  requestId: string;
  method: string;
  description: string;
  responded?: boolean;
  /** Human-readable name of the selected ACP option. */
  decision?: string;
  /** Exact ACP option ID selected by the user. */
  decisionOptionId?: string;
  title?: string;
  kind?: string;
  options?: AgentPermissionOption[];
}

/** The exact option object advertised by an ACP agent. */
export interface AgentPermissionOption {
  optionId: string;
  name: string;
  kind: string;
  [key: string]: unknown;
}

export type TurnEntry =
  | TurnEntryMessage
  | TurnEntryThought
  | TurnEntryTool
  | TurnEntryPlan
  | TurnEntryPermission
  | TurnEntryElicitation;

export interface DisplayTurn {
  id: number;
  type: 'turn';
  agent: string;
  timestamp: string;
  completedAt: string | null;
  status: 'in_progress' | 'complete';
  stopReason: string | null;
  entries: TurnEntry[];
}

export interface DisplayUserMessage {
  id: number;
  type: 'user_message';
  text: string;
  content?: RichContentBlock[];
  timestamp: string;
  messageId?: string;
}

export interface DisplayError {
  id: number;
  type: 'error';
  message: string;
  timestamp: string;
}

export type DisplayItem =
  | DisplayTurn
  | DisplayUserMessage
  | DisplayError;

export interface SessionEvent {
  seq: number;
  session_id: string;
  agent: string;
  timestamp: string;
  payload: SessionPayload;
}

export interface ChatHistoryPage {
  events: SessionEvent[];
  next_cursor: number | null;
  has_older: boolean;
}

export interface SessionPayload {
  type:
    | 'user_message'
    | 'message_chunk'
    | 'thought_chunk'
    | 'tool_call'
    | 'tool_call_update'
    | 'plan'
    | 'permission_request'
    | 'permission_response'
    | 'turn_complete'
    | 'state_change'
    | 'config_options'
    | 'available_commands'
    | 'session_modes'
    | 'usage_update'
    | 'session_info'
    | 'elicitation_request'
    | 'elicitation_response'
    | 'elicitation_complete'
    | 'error'
    | 'metadata_changed';
  id?: unknown;
  text?: unknown;
  content?: unknown;
  message?: unknown;
  title?: unknown;
  status?: unknown;
  output?: unknown;
  method?: unknown;
  description?: unknown;
  process?: unknown;
  turn?: unknown;
  stop_reason?: unknown;
  stopReason?: unknown;
  toolCallId?: unknown;
  tool_call_id?: unknown;
  options?: unknown;
  entries?: unknown;
  [key: string]: unknown;
}
