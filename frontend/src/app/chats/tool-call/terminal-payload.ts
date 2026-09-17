export type TerminalExecutionState = 'running' | 'completed' | 'failed';

export interface TerminalExecution {
  command?: string;
  workingDir?: string;
  exitCode?: number | null;
  output?: string;
  state: TerminalExecutionState;
  stateLabel: string;
  unrecognizedFields?: Record<string, unknown>;
}

export interface ParseTerminalOptions {
  toolStatus?: string | null;
  toolTitle?: string | null;
  toolKind?: string | null;
}

const COMMAND_KEYS = ['commandLine', 'command_line', 'command', 'cmd', 'exec'] as const;
const WORKDIR_KEYS = [
  'workingDir',
  'working_dir',
  'workdir',
  'workingDirectory',
  'working_directory',
  'cwd',
] as const;
const EXIT_CODE_KEYS = [
  'exitCode',
  'exit_code',
  'exitStatus',
  'exit_status',
  'statusCode',
  'status_code',
  'returnCode',
  'return_code',
] as const;
const OUTPUT_KEYS = [
  'combinedOutput',
  'combined_output',
  'formatted_output',
  'formattedOutput',
  'output',
  'stdout',
] as const;
const STATUS_KEYS = ['state', 'status', 'executionState', 'execution_state'] as const;

const NON_TERMINAL_KINDS = new Set(['read', 'edit', 'delete', 'think', 'search']);
const GENERIC_TITLES = new Set([
  'terminal',
  'tool call',
  'execute',
  'command',
  'run command',
  'execute command',
  'bash',
  'sh',
  'zsh',
]);

function stripCodeFences(text: string): string {
  const trimmed = text.trim();
  if (trimmed.startsWith('```') && trimmed.endsWith('```') && trimmed.length >= 6) {
    const inner = trimmed.slice(3, -3);
    const nl = inner.indexOf('\n');
    return (nl >= 0 ? inner.slice(nl + 1) : inner).trimEnd();
  }
  return text;
}

function cleanTitle(title?: string | null): string | undefined {
  if (!title) return undefined;
  const trimmed = title.trim();
  const stripped = trimmed.replace(/^(?:terminal|run command|execute command|execute):\s*/i, '').trim();
  if (!stripped || GENERIC_TITLES.has(stripped.toLowerCase())) {
    return undefined;
  }
  return stripped;
}

export function parseTerminalPayload(
  raw: unknown,
  options?: ParseTerminalOptions,
): TerminalExecution | null {
  const kind = (options?.toolKind || '').toLowerCase();
  if (NON_TERMINAL_KINDS.has(kind)) {
    return null;
  }

  let obj: Record<string, unknown> | null = null;
  if (raw && typeof raw === 'object' && !Array.isArray(raw)) {
    obj = raw as Record<string, unknown>;
  } else if (typeof raw === 'string') {
    const cleaned = stripCodeFences(raw).trim();
    if (cleaned.startsWith('{') && cleaned.endsWith('}')) {
      try {
        const parsed = JSON.parse(cleaned);
        if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
          obj = parsed as Record<string, unknown>;
        }
      } catch {
        return null;
      }
    }
  }

  if (!obj) {
    return null;
  }

  // Check if obj contains recognized terminal keys
  const hasCommandKey = COMMAND_KEYS.some((k) => typeof obj![k] === 'string' && (obj![k] as string).trim().length > 0);
  const hasWorkdirKey = WORKDIR_KEYS.some((k) => typeof obj![k] === 'string' && (obj![k] as string).trim().length > 0);
  const hasExitCodeKey = EXIT_CODE_KEYS.some((k) => {
    const val = obj![k];
    if (typeof val === 'number') return true;
    if (typeof val === 'string' && /^-?\d+$/.test(val.trim())) return true;
    return false;
  });
  const hasSpecificOutputKey = ['combinedOutput', 'combined_output', 'formatted_output', 'formattedOutput'].some(
    (k) => obj![k] !== undefined && obj![k] !== null,
  );
  const hasAnyOutputKey = OUTPUT_KEYS.some((k) => obj![k] !== undefined && obj![k] !== null);

  const isExplicitExecuteKind = kind === 'execute' || kind === 'terminal';
  const looksLikeTerminal =
    hasCommandKey ||
    hasExitCodeKey ||
    hasSpecificOutputKey ||
    (hasWorkdirKey && (hasAnyOutputKey || isExplicitExecuteKind)) ||
    (isExplicitExecuteKind && hasAnyOutputKey);

  if (!looksLikeTerminal) {
    return null;
  }

  // 1. Extract command
  let command: string | undefined;
  for (const k of COMMAND_KEYS) {
    const val = obj[k];
    if (typeof val === 'string' && val.trim().length > 0) {
      command = val.trim();
      break;
    }
  }
  if (!command) {
    command = cleanTitle(options?.toolTitle);
  }

  // 2. Extract working directory
  let workingDir: string | undefined;
  for (const k of WORKDIR_KEYS) {
    const val = obj[k];
    if (typeof val === 'string' && val.trim().length > 0) {
      workingDir = val.trim();
      break;
    }
  }

  // 3. Extract exit code
  let exitCode: number | null = null;
  for (const k of EXIT_CODE_KEYS) {
    const val = obj[k];
    if (typeof val === 'number' && Number.isFinite(val)) {
      exitCode = val;
      break;
    }
    if (typeof val === 'string' && /^-?\d+$/.test(val.trim())) {
      exitCode = parseInt(val.trim(), 10);
      break;
    }
  }

  // 4. Extract output body
  let output: string | undefined;
  for (const k of OUTPUT_KEYS) {
    const val = obj[k];
    if (val !== undefined && val !== null) {
      if (typeof val === 'string') {
        output = val;
      } else if (typeof val === 'object') {
        output = JSON.stringify(val, null, 2);
      } else {
        output = String(val);
      }
      break;
    }
  }
  // Check stderr if stdout was used and stderr has additional content
  if (output !== undefined && obj['stderr'] && typeof obj['stderr'] === 'string' && obj['stderr'].trim().length > 0) {
    const stderrStr = obj['stderr'] as string;
    if (!output.includes(stderrStr)) {
      output = output ? `${output}\n${stderrStr}` : stderrStr;
    }
  } else if (output === undefined && obj['stderr'] && typeof obj['stderr'] === 'string') {
    output = obj['stderr'] as string;
  }

  // 5. Determine execution state and label
  const toolStatus = (options?.toolStatus || '').toLowerCase();
  let state: TerminalExecutionState;
  let stateLabel: string;

  if (exitCode !== null) {
    if (exitCode === 0) {
      state = 'completed';
      stateLabel = 'Success';
    } else {
      state = 'failed';
      stateLabel = 'Failed';
    }
  } else {
    let payloadStatus: string | undefined;
    for (const k of STATUS_KEYS) {
      const val = obj[k];
      if (typeof val === 'string') {
        payloadStatus = val.toLowerCase();
        break;
      }
    }

    const effectiveStatus = payloadStatus || toolStatus;
    if (effectiveStatus === 'completed' || effectiveStatus === 'success') {
      state = 'completed';
      stateLabel = 'Completed';
    } else if (effectiveStatus === 'failed' || effectiveStatus === 'error') {
      state = 'failed';
      stateLabel = 'Failed';
    } else {
      state = 'running';
      stateLabel = 'Running';
    }
  }

  // 6. Handle redundant aliases vs unrecognized fields
  const recognizedKeys = new Set<string>([
    ...COMMAND_KEYS,
    ...WORKDIR_KEYS,
    ...EXIT_CODE_KEYS,
    ...OUTPUT_KEYS,
    ...STATUS_KEYS,
    'stderr',
  ]);

  const unrecognizedFields: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(obj)) {
    if (!recognizedKeys.has(k)) {
      unrecognizedFields[k] = v;
    }
  }

  return {
    command,
    workingDir,
    exitCode,
    output,
    state,
    stateLabel,
    ...(Object.keys(unrecognizedFields).length > 0 ? { unrecognizedFields } : {}),
  };
}
