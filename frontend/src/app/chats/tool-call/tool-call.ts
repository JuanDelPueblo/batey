import { Component, computed, input } from '@angular/core';
import { MatExpansionModule } from '@angular/material/expansion';
import { MatIconModule } from '@angular/material/icon';
import type { RichContentBlock, TurnEntryTool } from '../../core/api/types';
import { RichContentComponent } from '../rich-content/rich-content';
import { parseTerminalPayload } from './terminal-payload';

@Component({
  selector: 'hub-tool-call',
  imports: [MatExpansionModule, MatIconModule, RichContentComponent],
  templateUrl: './tool-call.html',
  styleUrl: './tool-call.scss',
})
export class ToolCallComponent {
  readonly tool = input.required<TurnEntryTool>();

  readonly terminal = computed(() => {
    return parseTerminalPayload(this.tool().output, {
      toolStatus: this.tool().status,
      toolTitle: this.tool().title,
      toolKind: this.tool().kind,
    });
  });

  readonly icon = computed(() => {
    if (this.terminal()) {
      return 'terminal';
    }
    const kind = (this.tool().kind || '').toLowerCase();
    switch (kind) {
      case 'read':
        return 'description';
      case 'execute':
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

  readonly isSubagentChild = computed(() => !!this.tool().parentId);

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

  readonly panelDescription = computed(() => {
    const term = this.terminal();
    if (term?.exitCode != null) {
      return term.exitCode === 0 ? 'completed (exit 0)' : `failed (exit ${term.exitCode})`;
    }
    return this.tool().status;
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
}
