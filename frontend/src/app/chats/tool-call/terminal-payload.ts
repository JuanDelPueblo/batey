export interface TerminalExecution {
  command?: string;
  workingDir?: string;
  exitCode: number | null;
  output?: string;
  state: TerminalExecutionState;
  stateLabel: string;
  unrecognizedFields?: Record<string, unknown>;
}

export type TerminalExecutionState = 'running' | 'completed' | 'failed';

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
  'stderr',
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

function normalizeOutputValue(val: unknown): string {
  if (typeof val === 'string') return val;
  if (typeof val === 'object' && val !== null) return JSON.stringify(val, null, 2);
  return String(val);
}

function normalizeExitCodeValue(val: unknown): number | null {
  if (typeof val === 'number' && Number.isFinite(val)) return val;
  if (typeof val === 'string' && /^-?\d+$/.test(val.trim())) return parseInt(val.trim(), 10);
  return null;
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
  const hasExitCodeKey = EXIT_CODE_KEYS.some((k) => normalizeExitCodeValue(obj![k]) !== null);
  const hasSpecificOutputKey = ['combinedOutput', 'combined_output', 'formatted_output', 'formattedOutput'].some(
    (k) => obj![k] !== undefined && obj![k] !== null,
  );
  const hasAnyOutputKey = OUTPUT_KEYS.some((k) => obj![k] !== undefined && obj![k] !== null);
  const hasNonBlankStatusKey = STATUS_KEYS.some(
    (k) => typeof obj![k] === 'string' && (obj![k] as string).trim().length > 0,
  );

  const isExplicitExecuteKind = kind === 'execute' || kind === 'terminal';
  const looksLikeTerminal =
    hasCommandKey ||
    hasExitCodeKey ||
    hasSpecificOutputKey ||
    (hasWorkdirKey && (hasAnyOutputKey || isExplicitExecuteKind)) ||
    (isExplicitExecuteKind && (hasAnyOutputKey || hasNonBlankStatusKey));

  if (!looksLikeTerminal) {
    return null;
  }

  const consumedKeys = new Set<string>();

  // 1. Extract command
  let command: string | undefined;
  for (const k of COMMAND_KEYS) {
    const val = obj[k];
    if (typeof val === 'string' && val.trim().length > 0) {
      command = val.trim();
      consumedKeys.add(k);
      break;
    }
  }
  if (!command) {
    command = cleanTitle(options?.toolTitle);
  } else {
    // Check other command keys: deduplicate only if normalized value equals primary command
    for (const k of COMMAND_KEYS) {
      if (consumedKeys.has(k) || obj[k] === undefined || obj[k] === null) continue;
      if (typeof obj[k] === 'string') {
        const trimmed = (obj[k] as string).trim();
        if (trimmed.length === 0 || trimmed === command) {
          consumedKeys.add(k);
        }
      }
    }
  }

  // 2. Extract working directory
  let workingDir: string | undefined;
  for (const k of WORKDIR_KEYS) {
    const val = obj[k];
    if (typeof val === 'string' && val.trim().length > 0) {
      workingDir = val.trim();
      consumedKeys.add(k);
      break;
    }
  }
  if (workingDir) {
    // Check other working dir keys: deduplicate only if normalized value equals primary workingDir
    for (const k of WORKDIR_KEYS) {
      if (consumedKeys.has(k) || obj[k] === undefined || obj[k] === null) continue;
      if (typeof obj[k] === 'string') {
        const trimmed = (obj[k] as string).trim();
        if (trimmed.length === 0 || trimmed === workingDir) {
          consumedKeys.add(k);
        }
      }
    }
  }

  // 3. Extract exit code
  let exitCode: number | null = null;
  for (const k of EXIT_CODE_KEYS) {
    const norm = normalizeExitCodeValue(obj[k]);
    if (norm !== null) {
      exitCode = norm;
      consumedKeys.add(k);
      break;
    }
  }
  if (exitCode !== null) {
    // Check other exit code keys: deduplicate only if normalized integer equals primary exitCode
    for (const k of EXIT_CODE_KEYS) {
      if (consumedKeys.has(k) || obj[k] === undefined || obj[k] === null) continue;
      const norm = normalizeExitCodeValue(obj[k]);
      if (norm !== null && norm === exitCode) {
        consumedKeys.add(k);
      }
    }
  }

  // 4. Extract output body
  let output: string | undefined;
  let selectedOutputKey: (typeof OUTPUT_KEYS)[number] | undefined;
  for (const k of OUTPUT_KEYS) {
    const val = obj[k];
    if (val !== undefined && val !== null) {
      output = normalizeOutputValue(val);
      selectedOutputKey = k;
      consumedKeys.add(k);
      break;
    }
  }
  if (output !== undefined) {
    // Check other output keys: deduplicate only if normalized string equals primary output
    for (const k of OUTPUT_KEYS) {
      if (k === 'stderr') continue;
      if (consumedKeys.has(k) || obj[k] === undefined || obj[k] === null) continue;
      const norm = normalizeOutputValue(obj[k]);
      if (norm === output) {
        consumedKeys.add(k);
      }
    }
  }
  // Check stderr handling
  if (obj['stderr'] !== undefined && obj['stderr'] !== null) {
    const stderrStr = normalizeOutputValue(obj['stderr']);
    if (selectedOutputKey !== 'stderr') {
      if (output !== undefined) {
        const isCombined =
          selectedOutputKey === 'combinedOutput' ||
          selectedOutputKey === 'combined_output' ||
          selectedOutputKey === 'formatted_output' ||
          selectedOutputKey === 'formattedOutput';
        if (isCombined) {
          if (!output.includes(stderrStr)) {
            output = output ? `${output}\n${stderrStr}` : stderrStr;
          }
        } else {
          output = output ? `${output}\n${stderrStr}` : stderrStr;
        }
      } else {
        output = stderrStr;
      }
    }
    consumedKeys.add('stderr');
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
      if (typeof val === 'string' && val.trim().length > 0) {
        payloadStatus = val.toLowerCase().trim();
        consumedKeys.add(k);
        break;
      }
    }
    if (payloadStatus) {
      for (const k of STATUS_KEYS) {
        if (consumedKeys.has(k) || obj[k] === undefined || obj[k] === null) continue;
        if (typeof obj[k] === 'string' && (obj[k] as string).toLowerCase().trim() === payloadStatus) {
          consumedKeys.add(k);
        }
      }
    }

    const effectiveStatus = payloadStatus || toolStatus;
    if (effectiveStatus === 'in_progress' || effectiveStatus === 'running' || effectiveStatus === 'pending') {
      state = 'running';
      stateLabel = 'Running';
    } else if (
      effectiveStatus === 'failed' ||
      effectiveStatus === 'error' ||
      effectiveStatus === 'rejected' ||
      effectiveStatus === 'cancelled'
    ) {
      state = 'failed';
      stateLabel = 'Failed';
    } else {
      state = 'completed';
      stateLabel = 'Success';
    }
  }

  // 6. Collect any unrecognized fields
  const unrecognizedFields: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(obj)) {
    if (!consumedKeys.has(key)) {
      unrecognizedFields[key] = value;
    }
  }

  return {
    command,
    workingDir,
    exitCode,
    output,
    state,
    stateLabel,
    unrecognizedFields: Object.keys(unrecognizedFields).length > 0 ? unrecognizedFields : undefined,
  };
}
