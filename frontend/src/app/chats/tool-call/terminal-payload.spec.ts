import { describe, expect, it } from 'vitest';
import { parseTerminalPayload } from './terminal-payload';

describe('parseTerminalPayload', () => {
  it('normalizes Antigravity payload with duplicate aliases into single values', () => {
    const raw = JSON.stringify({
      commandLine: 'cargo test --workspace',
      workingDir: '/srv/pool/apps/batey',
      exitCode: 0,
      exit_code: 0,
      combinedOutput: 'test result: ok. 543 passed',
      formatted_output: 'test result: ok. 543 passed',
    });

    const parsed = parseTerminalPayload(raw, {
      toolStatus: 'completed',
      toolTitle: 'Terminal: cargo test --workspace',
      toolKind: 'execute',
    });

    expect(parsed).not.toBeNull();
    expect(parsed?.command).toBe('cargo test --workspace');
    expect(parsed?.workingDir).toBe('/srv/pool/apps/batey');
    expect(parsed?.exitCode).toBe(0);
    expect(parsed?.output).toBe('test result: ok. 543 passed');
    expect(parsed?.state).toBe('completed');
    expect(parsed?.stateLabel).toBe('Success');
    expect(parsed?.unrecognizedFields).toBeUndefined();
  });

  it('normalizes Codex payload with formatted_output and exit_code', () => {
    const raw = JSON.stringify({
      formatted_output: 'Cargo.lock up to date\nAll tests passed',
      exit_code: 0,
    });

    const parsed = parseTerminalPayload(raw, {
      toolStatus: 'completed',
      toolTitle: 'Terminal: cargo test',
      toolKind: 'execute',
    });

    expect(parsed).not.toBeNull();
    expect(parsed?.command).toBe('cargo test');
    expect(parsed?.workingDir).toBeUndefined();
    expect(parsed?.exitCode).toBe(0);
    expect(parsed?.output).toBe('Cargo.lock up to date\nAll tests passed');
    expect(parsed?.state).toBe('completed');
    expect(parsed?.stateLabel).toBe('Success');
    expect(parsed?.unrecognizedFields).toBeUndefined();
  });

  it('preserves conflicting output alias values in unrecognizedFields', () => {
    const raw = JSON.stringify({
      commandLine: 'git diff',
      exitCode: 0,
      combinedOutput: 'stdout',
      formatted_output: 'formatted diagnostics',
    });

    const parsed = parseTerminalPayload(raw, {
      toolStatus: 'completed',
      toolTitle: 'git diff',
      toolKind: 'execute',
    });

    expect(parsed).not.toBeNull();
    expect(parsed?.output).toBe('stdout');
    expect(parsed?.unrecognizedFields).toEqual({
      formatted_output: 'formatted diagnostics',
    });
  });

  it('preserves conflicting exit-code alias values in unrecognizedFields', () => {
    const raw = JSON.stringify({
      commandLine: 'cargo test',
      exitCode: 0,
      exit_code: 1,
      output: 'done',
    });

    const parsed = parseTerminalPayload(raw, {
      toolStatus: 'completed',
      toolTitle: 'cargo test',
      toolKind: 'execute',
    });

    expect(parsed).not.toBeNull();
    expect(parsed?.exitCode).toBe(0);
    expect(parsed?.unrecognizedFields).toEqual({
      exit_code: 1,
    });
  });

  it('preserves conflicting command alias values in unrecognizedFields', () => {
    const raw = JSON.stringify({
      commandLine: 'npm test',
      cmd: 'npm run test:ci',
      exitCode: 0,
    });

    const parsed = parseTerminalPayload(raw, {
      toolStatus: 'completed',
      toolTitle: 'npm test',
      toolKind: 'execute',
    });

    expect(parsed).not.toBeNull();
    expect(parsed?.command).toBe('npm test');
    expect(parsed?.unrecognizedFields).toEqual({
      cmd: 'npm run test:ci',
    });
  });

  it('normalizes failed execution with non-zero exit code', () => {
    const raw = JSON.stringify({
      commandLine: 'cargo check',
      exit_code: 1,
      formatted_output: 'error: failed to compile',
    });

    const parsed = parseTerminalPayload(raw, {
      toolStatus: 'completed',
      toolTitle: 'cargo check',
      toolKind: 'execute',
    });

    expect(parsed).not.toBeNull();
    expect(parsed?.state).toBe('failed');
    expect(parsed?.stateLabel).toBe('Failed');
    expect(parsed?.exitCode).toBe(1);
    expect(parsed?.output).toBe('error: failed to compile');
  });

  it('normalizes running command with in_progress status and no exit code', () => {
    const raw = JSON.stringify({
      commandLine: 'npm run dev',
      workingDir: '/workspace',
    });

    const parsed = parseTerminalPayload(raw, {
      toolStatus: 'in_progress',
      toolTitle: 'Terminal: npm run dev',
      toolKind: 'execute',
    });

    expect(parsed).not.toBeNull();
    expect(parsed?.state).toBe('running');
    expect(parsed?.stateLabel).toBe('Running');
    expect(parsed?.exitCode).toBeNull();
    expect(parsed?.command).toBe('npm run dev');
    expect(parsed?.workingDir).toBe('/workspace');
    expect(parsed?.output).toBeUndefined();
  });

  it('preserves unrecognized fields without discarding data', () => {
    const raw = JSON.stringify({
      commandLine: 'git status',
      exitCode: 0,
      combinedOutput: 'clean',
      customEnvironment: 'ci-runner-1',
      durationMs: 450,
    });

    const parsed = parseTerminalPayload(raw, {
      toolStatus: 'completed',
      toolTitle: 'git status',
      toolKind: 'execute',
    });

    expect(parsed).not.toBeNull();
    expect(parsed?.command).toBe('git status');
    expect(parsed?.unrecognizedFields).toEqual({
      customEnvironment: 'ci-runner-1',
      durationMs: 450,
    });
  });

  it('recognizes explicit execute payload when it contains a nonblank status key', () => {
    const raw = JSON.stringify({
      status: 'in_progress',
    });

    const parsed = parseTerminalPayload(raw, {
      toolKind: 'execute',
      toolTitle: 'Run tests',
    });

    expect(parsed).not.toBeNull();
    expect(parsed?.state).toBe('running');
    expect(parsed?.stateLabel).toBe('Running');
    expect(parsed?.command).toBe('Run tests');
  });

  it('preserves NON_TERMINAL_KINDS guard before status key recognition', () => {
    const raw = JSON.stringify({
      status: 'in_progress',
    });

    expect(parseTerminalPayload(raw, { toolKind: 'read' })).toBeNull();
    expect(parseTerminalPayload(raw, { toolKind: 'think' })).toBeNull();
  });

  it('deduplicates stderr substring only when combined output is selected', () => {
    const combined = JSON.stringify({
      combinedOutput: 'main output\nerror details',
      stderr: 'error details',
    });
    const parsedCombined = parseTerminalPayload(combined, { toolKind: 'execute' });
    expect(parsedCombined).not.toBeNull();
    expect(parsedCombined?.output).toBe('main output\nerror details');
    expect(parsedCombined?.unrecognizedFields).toBeUndefined();
  });

  it('appends stderr when stdout or output is selected even if it appears as a substring', () => {
    const stdoutPayload = JSON.stringify({
      stdout: 'error details',
      stderr: 'error details',
    });
    const parsedStdout = parseTerminalPayload(stdoutPayload, { toolKind: 'execute' });
    expect(parsedStdout).not.toBeNull();
    expect(parsedStdout?.output).toBe('error details\nerror details');
    expect(parsedStdout?.unrecognizedFields).toBeUndefined();

    const outputPayload = JSON.stringify({
      output: 'error details',
      stderr: 'error details',
    });
    const parsedOutput = parseTerminalPayload(outputPayload, { toolKind: 'execute' });
    expect(parsedOutput).not.toBeNull();
    expect(parsedOutput?.output).toBe('error details\nerror details');
    expect(parsedOutput?.unrecognizedFields).toBeUndefined();
  });

  it('recognizes payload containing only stderr', () => {
    const raw = JSON.stringify({
      stderr: 'process crashed',
    });
    const parsed = parseTerminalPayload(raw, { toolKind: 'execute' });
    expect(parsed).not.toBeNull();
    expect(parsed?.output).toBe('process crashed');
  });

  it('returns null for non-terminal kinds such as read, edit, search', () => {
    const raw = JSON.stringify({
      command: 'echo fake',
      output: 'something',
    });

    expect(parseTerminalPayload(raw, { toolKind: 'read' })).toBeNull();
    expect(parseTerminalPayload(raw, { toolKind: 'edit' })).toBeNull();
    expect(parseTerminalPayload(raw, { toolKind: 'search' })).toBeNull();
  });

  it('returns null for non-terminal JSON structures', () => {
    const raw = JSON.stringify({
      query: 'SELECT * FROM users',
      results: [{ id: 1 }, { id: 2 }],
    });

    expect(parseTerminalPayload(raw, { toolKind: 'database' })).toBeNull();
  });

  it('returns null for plain text output that is not JSON', () => {
    expect(parseTerminalPayload('Finished dev profile', { toolKind: 'other' })).toBeNull();
    expect(parseTerminalPayload('fn main() {}', { toolKind: 'read' })).toBeNull();
    expect(parseTerminalPayload(null)).toBeNull();
    expect(parseTerminalPayload(undefined)).toBeNull();
  });

  it('handles markdown code fence wrapping JSON', () => {
    const raw = '```json\n{"commandLine": "ls", "exitCode": 0, "output": "a.txt"}\n```';
    const parsed = parseTerminalPayload(raw);

    expect(parsed).not.toBeNull();
    expect(parsed?.command).toBe('ls');
    expect(parsed?.exitCode).toBe(0);
    expect(parsed?.output).toBe('a.txt');
  });
});
