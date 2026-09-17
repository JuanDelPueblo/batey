import { Component, computed, input, signal } from '@angular/core';
import { MatIconModule } from '@angular/material/icon';
import type { RichContentBlock, TurnEntryTool } from '../../core/api/types';
import { PlanViewComponent } from '../plan-view/plan-view';
import { RichContentComponent } from '../rich-content/rich-content';
import { parseTerminalPayload } from './terminal-payload';

@Component({
  selector: 'hub-tool-call',
  imports: [MatIconModule, PlanViewComponent, RichContentComponent],
  templateUrl: './tool-call.html',
  styleUrl: './tool-call.scss',
})
export class ToolCallComponent {
  readonly tool = input.required<TurnEntryTool>();

  readonly taskList = computed(() => this.tool().taskList ?? null);

  readonly taskListDetailsJson = computed(() => {
    const details = this.taskList()?.details;
    return details?.length ? JSON.stringify(details, null, 2) : '';
  });

  private readonly userExpanded = signal<boolean | null>(null);

  readonly isSubagentChild = computed(() => !!this.tool().parentId);

  readonly terminal = computed(() => {
    return parseTerminalPayload(this.tool().output, {
      toolStatus: this.tool().status,
      toolTitle: this.tool().title,
      toolKind: this.tool().kind,
    });
  });

  readonly normalizedStatus = computed<'running' | 'completed' | 'failed'>(() => {
    const term = this.terminal();
    if (term) {
      return term.state;
    }
    const raw = (this.tool().status || '').toLowerCase().trim();
    if (raw === 'in_progress' || raw === 'running' || raw === 'pending') {
      return 'running';
    }
    if (raw === 'failed' || raw === 'error' || raw === 'rejected' || raw === 'cancelled') {
      return 'failed';
    }
    return 'completed';
  });

  readonly isRunning = computed(() => this.normalizedStatus() === 'running');
  readonly isFailed = computed(() => this.normalizedStatus() === 'failed');
  readonly isCompleted = computed(() => this.normalizedStatus() === 'completed');

  readonly statusLabel = computed(() => {
    const term = this.terminal();
    if (term?.exitCode != null) {
      return term.exitCode === 0 ? 'Completed (exit 0)' : `Failed (exit ${term.exitCode})`;
    }
    switch (this.normalizedStatus()) {
      case 'running':
        return 'Running';
      case 'failed':
        return 'Failed';
      case 'completed':
        return 'Completed';
    }
  });

  readonly panelDescription = computed(() => {
    const term = this.terminal();
    if (term) {
      if (term.exitCode != null) {
        return term.exitCode === 0 ? 'completed (exit 0)' : `failed (exit ${term.exitCode})`;
      }
      return term.state;
    }
    return this.tool().status;
  });

  readonly semanticKind = computed<string>(() => {
    if (this.terminal()) {
      return 'execute';
    }
    const explicit = (this.tool().kind || '').toLowerCase().trim();
    if (explicit) return explicit;

    const title = (this.tool().title || '').trim().toLowerCase();
    if (title.startsWith('read ') || title.startsWith('view ')) return 'read';
    if (title.startsWith('edit ') || title.startsWith('write ') || title.startsWith('patch ')) return 'edit';
    if (title.startsWith('run ') || title.startsWith('execute ') || title.startsWith('bash ') || title.startsWith('sh ') || title.startsWith('terminal')) return 'execute';
    if (title.startsWith('search ') || title.startsWith('find ') || title.startsWith('grep ') || title.startsWith('rg ')) return 'search';
    if (title.startsWith('delete ') || title.startsWith('remove ') || title.startsWith('rm ')) return 'delete';
    if (title.startsWith('think ') || title.startsWith('thought')) return 'think';
    return '';
  });

  readonly icon = computed(() => {
    switch (this.semanticKind()) {
      case 'read':
        return 'description';
      case 'execute':
      case 'terminal':
        return 'terminal';
      case 'think':
        return 'psychology';
      case 'edit':
        return 'edit_document';
      case 'delete':
        return 'delete';
      case 'search':
        return 'search';
      default:
        return 'build';
    }
  });

  readonly firstLocation = computed(() => {
    const locs = this.tool().locations;
    return locs && locs.length > 0 ? locs[0] : null;
  });

  readonly parsedActivity = computed<{ title: string; summary: string }>(() => {
    const rawTitle = (this.tool().title || '').trim();
    const loc = this.firstLocation();
    const locSummary = loc ? loc.path + (loc.line != null ? `:${loc.line}` : '') : '';
    const kind = this.semanticKind();
    const term = this.terminal();

    // 1. Check for common action prefixes
    const verbMatch = rawTitle.match(/^(Read|View|Edit|Write|Patch|Delete|Remove|Run|Execute|Terminal|Search(?:\s+for)?|Find|Grep)\s+(.+)$/i);
    if (verbMatch) {
      const verb = verbMatch[1].toLowerCase().startsWith('search') ? 'Search' : verbMatch[1];
      const action = verb.charAt(0).toUpperCase() + verb.slice(1).toLowerCase();
      const target = verbMatch[2].trim();
      return {
        title: action === 'Terminal' ? 'Run' : action,
        summary: target || locSummary,
      };
    }

    // 2. If terminal payload has a command
    if (term?.command) {
      return {
        title: 'Run',
        summary: term.command,
      };
    }

    // 3. If kind is execute and rawTitle is a command
    if (kind === 'execute' && rawTitle) {
      return {
        title: 'Run',
        summary: rawTitle,
      };
    }

    // 4. If kind is read and rawTitle looks like a path
    if (kind === 'read' && rawTitle && (rawTitle.includes('/') || rawTitle.includes('.'))) {
      return {
        title: 'Read',
        summary: rawTitle,
      };
    }

    // 5. If kind is edit and rawTitle looks like a path
    if (kind === 'edit' && rawTitle && (rawTitle.includes('/') || rawTitle.includes('.'))) {
      return {
        title: 'Edit',
        summary: rawTitle,
      };
    }

    // 6. If kind is delete and rawTitle looks like a path
    if (kind === 'delete' && rawTitle && (rawTitle.includes('/') || rawTitle.includes('.'))) {
      return {
        title: 'Delete',
        summary: rawTitle,
      };
    }

    // 7. If locations exist and rawTitle does not already include it
    if (locSummary && rawTitle && !rawTitle.includes(locSummary)) {
      return {
        title: rawTitle,
        summary: locSummary,
      };
    }

    // Fallback
    const fallbackTitle = rawTitle || (kind ? kind.charAt(0).toUpperCase() + kind.slice(1) : 'Tool call');
    return {
      title: fallbackTitle,
      summary: locSummary,
    };
  });

  readonly displayTitle = computed(() => this.parsedActivity().title);
  readonly displaySummary = computed(() => this.parsedActivity().summary);

  readonly cleanOutput = computed(() => {
    const raw = this.tool().output;
    if (!raw) return '';
    const trimmed = raw.trim();
    if (trimmed.startsWith('```') && trimmed.endsWith('```') && trimmed.length >= 6) {
      const inner = trimmed.slice(3, -3);
      const nl = inner.indexOf('\n');
      return (nl >= 0 ? inner.slice(nl + 1) : inner).trimEnd();
    }
    return raw;
  });

  readonly hasUnrecognizedFields = computed(() => {
    const fields = this.terminal()?.unrecognizedFields;
    return !!fields && Object.keys(fields).length > 0;
  });

  readonly unrecognizedFieldsJson = computed(() => {
    const fields = this.terminal()?.unrecognizedFields;
    return fields ? JSON.stringify(fields, null, 2) : '';
  });

  readonly richContent = computed(() => {
    const content = this.tool().content;
    if (!Array.isArray(content)) return [] as RichContentBlock[];
    return content.flatMap((item) => {
      if (!item || typeof item !== 'object') return [];
      const block = (item as { type?: unknown; content?: unknown }).type === 'content'
        ? (item as { content?: unknown }).content : item;
      return block && typeof block === 'object' && typeof (block as { type?: unknown }).type === 'string'
        ? [block as RichContentBlock] : [];
    });
  });

  readonly hasDetails = computed(() => {
    if (this.terminal()) {
      const term = this.terminal()!;
      return (
        !!term.output ||
        !!term.command ||
        !!term.workingDir ||
        term.exitCode != null ||
        this.hasUnrecognizedFields() ||
        this.richContent().length > 0 ||
        (this.tool().locations?.length ?? 0) > 0
      );
    }
    return !!this.cleanOutput() || this.richContent().length > 0 || (this.tool().locations?.length ?? 0) > 0;
  });

  readonly isExpanded = computed(() => {
    const manual = this.userExpanded();
    if (manual !== null) return manual;
    // Failed tools with details automatically expose their error output
    if (this.isFailed() && this.hasDetails()) {
      return true;
    }
    return false;
  });

  readonly detailsId = computed(() => `tool-details-${this.tool().toolCallId || this.tool().id}`);

  readonly accessibleHeaderLabel = computed(() => {
    const parts: string[] = [];
    if (this.isSubagentChild()) parts.push('Subagent');
    parts.push(this.displayTitle());
    if (this.displaySummary()) parts.push(this.displaySummary());
    parts.push(this.statusLabel());
    if (this.hasDetails()) {
      parts.push(this.isExpanded() ? 'expanded' : 'collapsed');
    }
    return parts.join(', ');
  });

  toggleExpanded(): void {
    if (!this.hasDetails()) return;
    this.userExpanded.set(!this.isExpanded());
  }
}
