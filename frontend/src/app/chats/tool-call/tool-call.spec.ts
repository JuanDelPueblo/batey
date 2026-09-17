import { ComponentFixture, TestBed } from '@angular/core/testing';
import { describe, expect, it, beforeEach } from 'vitest';
import { ToolCallComponent } from './tool-call';
import type { TurnEntryTool } from '../../core/api/types';

describe('ToolCallComponent', () => {
  let fixture: ComponentFixture<ToolCallComponent>;
  let component: ToolCallComponent;

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [ToolCallComponent],
    }).compileComponents();
    fixture = TestBed.createComponent(ToolCallComponent);
    component = fixture.componentInstance;
  });

  it('displays read icon for read kind and strips markdown fence from output', () => {
    const tool: TurnEntryTool = {
      id: 1,
      type: 'tool_call',
      toolCallId: 't-1',
      title: 'Read src/main.rs',
      kind: 'read',
      status: 'completed',
      output: '```rust\nfn main() {}\n```',
    };
    fixture.componentRef.setInput('tool', tool);
    fixture.detectChanges();

    expect(component.icon()).toBe('description');
    expect(component.isSubagentChild()).toBe(false);
    expect(component.cleanOutput()).toBe('fn main() {}');

    const titleEl = fixture.nativeElement.querySelector('.tool-title');
    expect(titleEl.textContent).toBe('Read src/main.rs');

    const badge = fixture.nativeElement.querySelector('.subagent-badge');
    expect(badge).toBeNull();

    const outputPre = fixture.nativeElement.querySelector('.tool-output');
    expect(outputPre).not.toBeNull();
    expect(outputPre.textContent).toBe('fn main() {}');
    expect(fixture.nativeElement.querySelector('.terminal-summary')).toBeNull();
  });

  it('displays terminal icon for execute kind and shows subagent badge if parentId is set', () => {
    const tool: TurnEntryTool = {
      id: 2,
      type: 'tool_call',
      toolCallId: 't-2',
      title: 'cargo check',
      kind: 'execute',
      parentId: 'task-parent-1',
      status: 'completed',
      output: 'Finished dev profile',
    };

    fixture.componentRef.setInput('tool', tool);
    fixture.detectChanges();

    expect(component.icon()).toBe('terminal');
    expect(component.isSubagentChild()).toBe(true);

    const badge = fixture.nativeElement.querySelector('.subagent-badge');
    expect(badge).not.toBeNull();
    expect(badge.textContent).toBe('Subagent');

    const outputPre = fixture.nativeElement.querySelector('.tool-output');
    expect(outputPre).not.toBeNull();
    expect(outputPre.textContent).toBe('Finished dev profile');
  });

  it('renders Antigravity sample with commandLine, workingDir, duplicate exit codes, and duplicate outputs', () => {
    const rawPayload = JSON.stringify({
      commandLine: 'npm test -- --watch=false',
      workingDir: '/srv/pool/apps/batey/frontend',
      exitCode: 0,
      exit_code: 0,
      combinedOutput: 'PASS src/app/chats/tool-call/tool-call.spec.ts\nTests: 2 passed, 2 total',
      formatted_output: 'PASS src/app/chats/tool-call/tool-call.spec.ts\nTests: 2 passed, 2 total',
    });

    const tool: TurnEntryTool = {
      id: 3,
      type: 'tool_call',
      toolCallId: 't-3',
      title: 'Terminal: npm test -- --watch=false',
      kind: 'execute',
      status: 'completed',
      output: rawPayload,
    };

    fixture.componentRef.setInput('tool', tool);
    fixture.detectChanges();

    expect(component.icon()).toBe('terminal');

    // Summary renders one command
    const commandEls = fixture.nativeElement.querySelectorAll('.terminal-command');
    expect(commandEls.length).toBe(1);
    expect(commandEls[0].textContent).toBe('npm test -- --watch=false');

    // Summary renders one working directory
    const cwdEls = fixture.nativeElement.querySelectorAll('.terminal-cwd');
    expect(cwdEls.length).toBe(1);
    expect(cwdEls[0].textContent).toContain('/srv/pool/apps/batey/frontend');

    // Summary renders one exit status
    const badgeEl = fixture.nativeElement.querySelector('.terminal-status-badge');
    expect(badgeEl).not.toBeNull();
    expect(badgeEl.textContent).toContain('Success');
    expect(badgeEl.textContent).toContain('exit 0');
    expect(fixture.nativeElement.querySelectorAll('.terminal-status-badge').length).toBe(1);

    // Terminal output renders one output body, not raw JSON
    const terminalOutputEls = fixture.nativeElement.querySelectorAll('.terminal-output');
    expect(terminalOutputEls.length).toBe(1);
    expect(terminalOutputEls[0].textContent).toBe(
      'PASS src/app/chats/tool-call/tool-call.spec.ts\nTests: 2 passed, 2 total',
    );
    expect(fixture.nativeElement.textContent).not.toContain('"combinedOutput"');
    expect(fixture.nativeElement.textContent).not.toContain('"formatted_output"');

    // No extra unrecognized fields
    expect(fixture.nativeElement.querySelector('.terminal-extra-details')).toBeNull();
  });

  it('renders Codex sample {formatted_output, exit_code} as terminal output rather than JSON', () => {
    const rawPayload = JSON.stringify({
      formatted_output: 'Cargo.lock up to date\nAll tests passed',
      exit_code: 0,
    });

    const tool: TurnEntryTool = {
      id: 4,
      type: 'tool_call',
      toolCallId: 't-4',
      title: 'Terminal: cargo test',
      status: 'completed',
      output: rawPayload,
    };

    fixture.componentRef.setInput('tool', tool);
    fixture.detectChanges();

    expect(component.icon()).toBe('terminal');

    // Shows command from title
    const commandEl = fixture.nativeElement.querySelector('.terminal-command');
    expect(commandEl).not.toBeNull();
    expect(commandEl.textContent).toBe('cargo test');

    // No working dir was supplied, so it should not render
    expect(fixture.nativeElement.querySelector('.terminal-cwd')).toBeNull();

    // Renders terminal output rather than serialized JSON
    const outputEl = fixture.nativeElement.querySelector('.terminal-output');
    expect(outputEl).not.toBeNull();
    expect(outputEl.textContent).toBe('Cargo.lock up to date\nAll tests passed');
    expect(fixture.nativeElement.textContent).not.toContain('{"formatted_output"');

    // Exit status
    const badgeEl = fixture.nativeElement.querySelector('.terminal-status-badge');
    expect(badgeEl.textContent).toContain('Success');
    expect(badgeEl.textContent).toContain('exit 0');
  });

  it('distinguishes running, successful, and failed commands without relying on color alone', () => {
    // 1. Failed command
    const failedTool: TurnEntryTool = {
      id: 5,
      type: 'tool_call',
      toolCallId: 't-5',
      title: 'Terminal: cargo test',
      kind: 'execute',
      status: 'completed',
      output: JSON.stringify({
        commandLine: 'cargo test',
        exit_code: 1,
        formatted_output: 'test failed: assertion `left == right` failed',
      }),
    };

    fixture.componentRef.setInput('tool', failedTool);
    fixture.detectChanges();

    const failedBadge = fixture.nativeElement.querySelector('.terminal-status-badge');
    expect(failedBadge.classList.contains('status-failed')).toBe(true);
    expect(failedBadge.textContent).toContain('Failed');
    expect(failedBadge.textContent).toContain('exit 1');
    const failedIcon = failedBadge.querySelector('.status-icon');
    expect(failedIcon.textContent).toBe('error');

    // 2. Running command
    const runningTool: TurnEntryTool = {
      id: 6,
      type: 'tool_call',
      toolCallId: 't-6',
      title: 'Terminal: npm run dev',
      kind: 'execute',
      status: 'in_progress',
      output: JSON.stringify({
        commandLine: 'npm run dev',
        workingDir: '/app',
      }),
    };

    fixture.componentRef.setInput('tool', runningTool);
    fixture.detectChanges();

    const runningBadge = fixture.nativeElement.querySelector('.terminal-status-badge');
    expect(runningBadge.classList.contains('status-running')).toBe(true);
    expect(runningBadge.textContent).toContain('Running');
    expect(runningBadge.textContent).not.toContain('exit');
    const runningIcon = runningBadge.querySelector('.status-icon');
    expect(runningIcon.textContent).toBe('sync');

    // 3. Successful command
    const successTool: TurnEntryTool = {
      id: 7,
      type: 'tool_call',
      toolCallId: 't-7',
      title: 'Terminal: ls',
      kind: 'execute',
      status: 'completed',
      output: JSON.stringify({
        commandLine: 'ls',
        exitCode: 0,
        combinedOutput: 'file.txt',
      }),
    };

    fixture.componentRef.setInput('tool', successTool);
    fixture.detectChanges();

    const successBadge = fixture.nativeElement.querySelector('.terminal-status-badge');
    expect(successBadge.classList.contains('status-success')).toBe(true);
    expect(successBadge.textContent).toContain('Success');
    expect(successBadge.textContent).toContain('exit 0');
    const successIcon = successBadge.querySelector('.status-icon');
    expect(successIcon.textContent).toBe('check_circle');
  });

  it('renders unknown tool calls safely using fallback without discarding data', () => {
    const unknownTool: TurnEntryTool = {
      id: 8,
      type: 'tool_call',
      toolCallId: 't-8',
      title: 'Query database',
      kind: 'query',
      status: 'completed',
      output: '{"query": "SELECT * FROM users", "rows": [{"id": 1}]}',
    };

    fixture.componentRef.setInput('tool', unknownTool);
    fixture.detectChanges();

    expect(fixture.nativeElement.querySelector('.terminal-summary')).toBeNull();
    const fallbackPre = fixture.nativeElement.querySelector('.tool-output');
    expect(fallbackPre).not.toBeNull();
    expect(fallbackPre.textContent).toBe('{"query": "SELECT * FROM users", "rows": [{"id": 1}]}');
  });

  it('preserves unrecognized fields in details surface without discarding data', () => {
    const toolWithExtra: TurnEntryTool = {
      id: 9,
      type: 'tool_call',
      toolCallId: 't-9',
      title: 'Terminal: echo hi',
      kind: 'execute',
      status: 'completed',
      output: JSON.stringify({
        commandLine: 'echo hi',
        exitCode: 0,
        combinedOutput: 'hi',
        runnerId: 'runner-xyz',
        resourceUsage: { cpu: '12%' },
      }),
    };

    fixture.componentRef.setInput('tool', toolWithExtra);
    fixture.detectChanges();

    expect(fixture.nativeElement.querySelector('.terminal-command').textContent).toBe('echo hi');
    expect(fixture.nativeElement.querySelector('.terminal-output').textContent).toBe('hi');

    const extraDetails = fixture.nativeElement.querySelector('.terminal-extra-details');
    expect(extraDetails).not.toBeNull();
    expect(extraDetails.textContent).toContain('runner-xyz');
    expect(extraDetails.textContent).toContain('12%');
  });

  it('keeps long commands and long output readable with scrollable styling', () => {
    const longCommand = 'git log --graph --oneline --decorate --all --color=never ' + 'x'.repeat(300);
    const longOutput = Array.from({ length: 50 }, (_, i) => `log line ${i}: ${'y'.repeat(100)}`).join('\n');

    const tool: TurnEntryTool = {
      id: 10,
      type: 'tool_call',
      toolCallId: 't-10',
      title: 'Terminal: ' + longCommand,
      kind: 'execute',
      status: 'completed',
      output: JSON.stringify({
        commandLine: longCommand,
        exitCode: 0,
        formatted_output: longOutput,
      }),
    };

    fixture.componentRef.setInput('tool', tool);
    fixture.detectChanges();

    const cmdEl = fixture.nativeElement.querySelector('.terminal-command');
    expect(cmdEl).not.toBeNull();
    expect(cmdEl.textContent).toBe(longCommand);

    const outEl = fixture.nativeElement.querySelector('.terminal-output');
    expect(outEl).not.toBeNull();
    expect(outEl.textContent).toBe(longOutput);
  });

  it('preserves conflicting output alias values in expandable additional details', () => {
    const conflictingOutputTool: TurnEntryTool = {
      id: 11,
      type: 'tool_call',
      toolCallId: 't-11',
      title: 'Terminal: git diff',
      kind: 'execute',
      status: 'completed',
      output: JSON.stringify({
        commandLine: 'git diff',
        exitCode: 0,
        combinedOutput: 'stdout diff',
        formatted_output: 'formatted diagnostics',
      }),
    };

    fixture.componentRef.setInput('tool', conflictingOutputTool);
    fixture.detectChanges();

    const outputEl = fixture.nativeElement.querySelector('.terminal-output');
    expect(outputEl.textContent).toBe('stdout diff');

    const extraDetails = fixture.nativeElement.querySelector('.terminal-extra-details');
    expect(extraDetails).not.toBeNull();
    expect(extraDetails.textContent).toContain('formatted_output');
    expect(extraDetails.textContent).toContain('formatted diagnostics');
  });

  it('preserves conflicting exit-code alias values in expandable additional details', () => {
    const conflictingExitTool: TurnEntryTool = {
      id: 12,
      type: 'tool_call',
      toolCallId: 't-12',
      title: 'Terminal: cargo test',
      kind: 'execute',
      status: 'completed',
      output: JSON.stringify({
        commandLine: 'cargo test',
        exitCode: 0,
        exit_code: 1,
        output: 'test ran',
      }),
    };

    fixture.componentRef.setInput('tool', conflictingExitTool);
    fixture.detectChanges();

    const badgeEl = fixture.nativeElement.querySelector('.terminal-status-badge');
    expect(badgeEl.textContent).toContain('exit 0');

    const extraDetails = fixture.nativeElement.querySelector('.terminal-extra-details');
    expect(extraDetails).not.toBeNull();
    expect(extraDetails.textContent).toContain('exit_code');
    expect(extraDetails.textContent).toContain('1');
  });
});
