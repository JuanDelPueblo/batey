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

  it('displays read icon for read kind, concise title, secondary summary, and strips fence', () => {
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
    expect(titleEl.textContent.trim()).toBe('Read');

    const summaryEl = fixture.nativeElement.querySelector('.tool-summary');
    expect(summaryEl.textContent.trim()).toBe('src/main.rs');

    const badge = fixture.nativeElement.querySelector('.subagent-badge');
    expect(badge).toBeNull();
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
    expect(badge.textContent).toContain('Subagent');

    const activityEl = fixture.nativeElement.querySelector('.tool-activity');
    expect(activityEl.classList.contains('subagent-tool')).toBe(true);
  });

  it('handles running state with prominent status and spinner', () => {
    const tool: TurnEntryTool = {
      id: 3,
      type: 'tool_call',
      toolCallId: 't-3',
      title: 'Run npm test',
      kind: 'execute',
      status: 'in_progress',
    };

    fixture.componentRef.setInput('tool', tool);
    fixture.detectChanges();

    expect(component.isRunning()).toBe(true);
    expect(component.isCompleted()).toBe(false);
    expect(component.isFailed()).toBe(false);

    const activity = fixture.nativeElement.querySelector('.tool-activity');
    expect(activity.classList.contains('status-running')).toBe(true);

    const statusIndicator = fixture.nativeElement.querySelector('.status-indicator-running');
    expect(statusIndicator).not.toBeNull();
    expect(statusIndicator.textContent).toContain('Running');
    expect(statusIndicator.querySelector('.spin-icon')).not.toBeNull();
  });

  it('handles completed state with visually quiet indicator', () => {
    const tool: TurnEntryTool = {
      id: 4,
      type: 'tool_call',
      toolCallId: 't-4',
      title: 'Search files for pattern',
      kind: 'search',
      status: 'completed',
      output: 'Found 3 matches',
    };

    fixture.componentRef.setInput('tool', tool);
    fixture.detectChanges();

    expect(component.isCompleted()).toBe(true);
    expect(component.isRunning()).toBe(false);
    expect(component.isFailed()).toBe(false);

    const activity = fixture.nativeElement.querySelector('.tool-activity');
    expect(activity.classList.contains('status-completed')).toBe(true);

    const statusIndicator = fixture.nativeElement.querySelector('.status-indicator-completed');
    expect(statusIndicator).not.toBeNull();
    expect(statusIndicator.querySelector('.status-icon')?.textContent?.trim()).toBe('check');

    // Completed tool is collapsed by default
    expect(component.isExpanded()).toBe(false);
    expect(fixture.nativeElement.querySelector('.tool-details')).toBeNull();
  });

  it('handles failed state prominently and auto-expands useful error output', () => {
    const tool: TurnEntryTool = {
      id: 5,
      type: 'tool_call',
      toolCallId: 't-5',
      title: 'Run cargo build',
      kind: 'execute',
      status: 'failed',
      output: 'error[E0425]: cannot find value `foo` in this scope',
    };

    fixture.componentRef.setInput('tool', tool);
    fixture.detectChanges();

    expect(component.isFailed()).toBe(true);

    const activity = fixture.nativeElement.querySelector('.tool-activity');
    expect(activity.classList.contains('status-failed')).toBe(true);

    const statusIndicator = fixture.nativeElement.querySelector('.status-indicator-failed');
    expect(statusIndicator).not.toBeNull();
    expect(statusIndicator.textContent).toContain('Failed');

    // Failure with output should auto-expand by default
    expect(component.isExpanded()).toBe(true);
    const details = fixture.nativeElement.querySelector('.tool-details');
    expect(details).not.toBeNull();
    const output = fixture.nativeElement.querySelector('.tool-output');
    expect(output.textContent).toContain('cannot find value `foo`');
  });

  it('toggles expansion on header click and maintains keyboard accessibility', () => {
    const tool: TurnEntryTool = {
      id: 6,
      type: 'tool_call',
      toolCallId: 't-6',
      title: 'Edit src/app/test.ts',
      kind: 'edit',
      status: 'completed',
      output: 'Line 1 replaced',
    };

    fixture.componentRef.setInput('tool', tool);
    fixture.detectChanges();

    const headerBtn = fixture.nativeElement.querySelector('.tool-header') as HTMLButtonElement;
    expect(headerBtn.disabled).toBe(false);
    expect(headerBtn.getAttribute('aria-expanded')).toBe('false');
    expect(fixture.nativeElement.querySelector('.tool-details')).toBeNull();

    // Click to expand
    headerBtn.click();
    fixture.detectChanges();

    expect(component.isExpanded()).toBe(true);
    expect(headerBtn.getAttribute('aria-expanded')).toBe('true');
    expect(fixture.nativeElement.querySelector('.tool-details')).not.toBeNull();

    // Click to collapse
    headerBtn.click();
    fixture.detectChanges();

    expect(component.isExpanded()).toBe(false);
    expect(headerBtn.getAttribute('aria-expanded')).toBe('false');
    expect(fixture.nativeElement.querySelector('.tool-details')).toBeNull();
  });

  it('disables expansion when no output, content, or locations exist', () => {
    const tool: TurnEntryTool = {
      id: 7,
      type: 'tool_call',
      toolCallId: 't-7',
      title: 'Think about architecture',
      kind: 'think',
      status: 'completed',
    };

    fixture.componentRef.setInput('tool', tool);
    fixture.detectChanges();

    expect(component.hasDetails()).toBe(false);
    const headerBtn = fixture.nativeElement.querySelector('.tool-header') as HTMLButtonElement;
    expect(headerBtn.disabled).toBe(true);
    expect(headerBtn.getAttribute('aria-expanded')).toBeNull();
    expect(fixture.nativeElement.querySelector('.expand-icon')).toBeNull();
  });

  it('maps semantic kinds to familiar Material icons', () => {
    const kindsAndIcons: Array<[string, string]> = [
      ['read', 'description'],
      ['execute', 'terminal'],
      ['edit', 'edit_document'],
      ['delete', 'delete'],
      ['search', 'search'],
      ['think', 'psychology'],
      ['unknown_kind', 'build'],
    ];

    for (const [kind, expectedIcon] of kindsAndIcons) {
      fixture.componentRef.setInput('tool', {
        id: 10,
        type: 'tool_call',
        toolCallId: 't-10',
        title: `Test ${kind}`,
        kind,
        status: 'completed',
      });
      fixture.detectChanges();
      expect(component.icon()).toBe(expectedIcon);
    }
  });

  it('renders locations and rich content when expanded', () => {
    const tool: TurnEntryTool = {
      id: 8,
      type: 'tool_call',
      toolCallId: 't-8',
      title: 'Read src/app.ts',
      kind: 'read',
      status: 'completed',
      locations: [
        { path: 'src/app.ts', line: 42 },
        { path: 'src/app.ts', line: 99 },
      ],
      content: [{ type: 'text', text: 'Rich explanation' }],
    };

    fixture.componentRef.setInput('tool', tool);
    fixture.detectChanges();

    const headerBtn = fixture.nativeElement.querySelector('.tool-header') as HTMLButtonElement;
    headerBtn.click();
    fixture.detectChanges();

    const locationEls = fixture.nativeElement.querySelectorAll('.location');
    expect(locationEls.length).toBe(2);
    expect(locationEls[0]?.textContent).toContain('src/app.ts:42');
    expect(locationEls[1]?.textContent).toContain('src/app.ts:99');

    const richContent = fixture.nativeElement.querySelector('hub-rich-content');
    expect(richContent).not.toBeNull();
  });
});
