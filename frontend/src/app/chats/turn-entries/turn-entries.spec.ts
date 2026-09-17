import { ComponentFixture, TestBed } from '@angular/core/testing';
import { describe, expect, it, beforeEach, vi } from 'vitest';
import { TurnEntriesComponent } from './turn-entries';
import { AppStateService } from '../../state/app-state.service';
import type { TurnEntry } from '../../core/api/types';

describe('TurnEntriesComponent', () => {
  let fixture: ComponentFixture<TurnEntriesComponent>;

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [TurnEntriesComponent],
      providers: [
        {
          provide: AppStateService,
          useValue: { respondPermission: vi.fn(async () => undefined) },
        },
      ],
    }).compileComponents();
    fixture = TestBed.createComponent(TurnEntriesComponent);
  });

  it('renders a compact activity timeline for a sequence of completed and running tool calls', () => {
    const entries: TurnEntry[] = [
      {
        id: 1,
        type: 'tool_call',
        toolCallId: 't-1',
        title: 'Read src/main.rs',
        kind: 'read',
        status: 'completed',
        output: 'fn main() {}',
      },
      {
        id: 2,
        type: 'tool_call',
        toolCallId: 't-2',
        title: 'cargo check',
        kind: 'execute',
        status: 'completed',
        output: 'Finished dev profile',
      },
      {
        id: 3,
        type: 'tool_call',
        toolCallId: 't-3',
        title: 'Read backend/src/web/hub.rs',
        kind: 'read',
        status: 'in_progress',
      },
      {
        id: 4,
        type: 'tool_call',
        toolCallId: 't-4',
        title: 'Edit backend/src/web/hub.rs',
        kind: 'edit',
        parentId: 't-3',
        status: 'completed',
      },
    ];

    fixture.componentRef.setInput('entries', entries);
    fixture.detectChanges();

    const toolCalls = fixture.nativeElement.querySelectorAll('hub-tool-call');
    expect(toolCalls.length).toBe(4);

    // First two are completed
    expect(toolCalls[0].querySelector('.status-completed')).not.toBeNull();
    expect(toolCalls[1].querySelector('.status-completed')).not.toBeNull();

    // Third is in progress / running
    expect(toolCalls[2].querySelector('.status-running')).not.toBeNull();
    expect(toolCalls[2].querySelector('.status-indicator-running')?.textContent).toContain('Running');

    // Fourth is a subagent child tool call
    expect(toolCalls[3].querySelector('.subagent-tool')).not.toBeNull();
    expect(toolCalls[3].querySelector('.subagent-badge')?.textContent).toContain('Subagent');
  });

  it('renders short reasoning inline alongside message chunks and tool calls', () => {
    const entries: TurnEntry[] = [
      {
        id: 1,
        type: 'thought_chunk',
        text: 'Examining project setup',
      },
      {
        id: 2,
        type: 'tool_call',
        toolCallId: 't-1',
        title: 'cargo build',
        kind: 'execute',
        status: 'completed',
      },
      {
        id: 3,
        type: 'message_chunk',
        text: 'Build succeeded.',
      },
    ];

    fixture.componentRef.setInput('entries', entries);
    fixture.detectChanges();

    const reasoning = fixture.nativeElement.querySelector('.reasoning-inline') as HTMLElement;
    expect(reasoning).not.toBeNull();
    expect(reasoning.getAttribute('aria-label')).toBe('Agent reasoning');
    expect(reasoning.textContent).toContain('Examining project setup');
    expect(fixture.nativeElement.querySelector('mat-expansion-panel')).toBeNull();
    expect(fixture.nativeElement.querySelector('hub-tool-call')).not.toBeNull();
    expect(fixture.nativeElement.querySelector('.message-text')?.textContent).toContain('Build succeeded.');
  });

  it('collapses long reasoning behind a compact disclosure without losing streamed text', () => {
    const text = 'Inspecting the repository structure. '.repeat(12);
    fixture.componentRef.setInput('entries', [{ id: 1, type: 'thought_chunk', text }]);
    fixture.detectChanges();

    const reasoning = fixture.nativeElement.querySelector('.reasoning-details') as HTMLDetailsElement;
    expect(reasoning).not.toBeNull();
    expect(reasoning.open).toBe(false);
    expect(reasoning.querySelector('summary')?.textContent).toContain('Agent reasoning');
    expect(reasoning.querySelector('.reasoning-preview')?.textContent).toContain('Inspecting the repository');

    (reasoning.querySelector('summary') as HTMLElement).click();
    fixture.detectChanges();
    expect(reasoning.open).toBe(true);
    expect(reasoning.querySelector('.reasoning-content')?.textContent).toContain(text);
  });
});
