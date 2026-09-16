import { Component, computed, effect, ElementRef, inject, input, signal, viewChild } from '@angular/core';

import { FormControl, ReactiveFormsModule } from '@angular/forms';
import { TextFieldModule } from '@angular/cdk/text-field';
import { toSignal } from '@angular/core/rxjs-interop';
import { MatButtonModule } from '@angular/material/button';
import { MatIconModule } from '@angular/material/icon';
import { MatMenuModule } from '@angular/material/menu';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import { MatTooltipModule } from '@angular/material/tooltip';
import type { AvailableCommand, ConfigOption, ConfigOptionSelectGroup, ConfigOptionSelectValue, RichContentBlock, TurnState } from '../../core/api/types';
import { AppStateService } from '../../state/app-state.service';

const IMAGE_TYPES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp'] as const;
const AUDIO_TYPES = ['audio/mpeg', 'audio/wav', 'audio/ogg', 'audio/webm'] as const;
const RESOURCE_TYPES = ['text/plain', 'text/markdown', 'application/json'] as const;
const ATTACHMENT_ACCEPT = [...IMAGE_TYPES, ...AUDIO_TYPES, ...RESOURCE_TYPES, '.txt', '.md', '.json'].join(',');
const MAX_RESOURCE_BYTES = 512 * 1024;
const MAX_BINARY_BYTES = 2 * 1024 * 1024;
const MAX_TOTAL_BYTES = 4 * 1024 * 1024;

type AttachmentKind = 'image' | 'audio' | 'resource';

function resolveAttachmentKind(type: string): AttachmentKind | null {
  const normalized = type.toLowerCase();
  if ((IMAGE_TYPES as readonly string[]).includes(normalized)) return 'image';
  if ((AUDIO_TYPES as readonly string[]).includes(normalized)) return 'audio';
  if ((RESOURCE_TYPES as readonly string[]).includes(normalized)) return 'resource';
  return null;
}

@Component({
  selector: 'hub-chat-composer',
  imports: [ReactiveFormsModule, TextFieldModule, MatButtonModule, MatIconModule, MatMenuModule, MatProgressSpinnerModule, MatTooltipModule],
  templateUrl: './chat-composer.html',
  styleUrl: './chat-composer.scss',
})
export class ChatComposerComponent {
  readonly chatId = input('');
  readonly turnState = input<TurnState>('IDLE');
  readonly disabled = input(false);
  readonly options = input<ConfigOption[]>([]);
  readonly commands = input<AvailableCommand[]>([]);

  readonly message = new FormControl('', { nonNullable: true });
  readonly attachments = signal<RichContentBlock[]>([]);
  readonly attachmentError = signal<string | null>(null);
  readonly attachmentAccept = ATTACHMENT_ACCEPT;
  private readonly state = inject(AppStateService);
  private readonly text = toSignal(this.message.valueChanges, { initialValue: this.message.value });
  private readonly messageInput = viewChild<ElementRef<HTMLTextAreaElement>>('messageInput');
  private readonly commandList = viewChild<ElementRef<HTMLElement>>('commandList');
  private readonly highlightedIndex = signal(0);
  private readonly dismissedQuery = signal<string | null>(null);

  readonly prompting = computed(() => this.turnState() === 'PROMPTING');
  readonly cancelling = computed(() => this.turnState() === 'CANCELLING');
  readonly canConfigure = computed(() => !this.disabled() && this.turnState() === 'IDLE');
  readonly modelOption = computed(() => this.findOption('model', 'model', 'model'));
  readonly reasoningOption = computed(() => this.findOption('reasoning_effort', 'reasoning effort', 'thought_level'));

  readonly commandQuery = computed<string | null>(() => {
    const match = /^\/(\S*)$/.exec(this.text());
    return match ? match[1].toLowerCase() : null;
  });
  readonly filteredCommands = computed(() => {
    const query = this.commandQuery();
    if (query === null) return [];
    return this.commands()
      .filter((c) => !query || c.name.toLowerCase().startsWith(query));
  });
  readonly unavailable = computed(
    () => this.disabled() || this.prompting() || this.cancelling(),
  );
  readonly showCommands = computed(
    () =>
      this.filteredCommands().length > 0 &&
      this.commandQuery() !== this.dismissedQuery() &&
      !this.unavailable(),
  );
  readonly highlightedCommand = computed(() => {
    const list = this.filteredCommands();
    if (!list.length) return null;
    return list[Math.min(this.highlightedIndex(), list.length - 1)];
  });
  readonly commandListId = computed(() => `command-list-${this.chatId()}`);
  readonly activeCommandId = computed(() => {
    if (!this.showCommands()) return null;
    const list = this.filteredCommands();
    if (!list.length) return null;
    return this.commandOptionId(Math.min(this.highlightedIndex(), list.length - 1));
  });
  readonly canSend = computed(() => !this.unavailable() && (this.text().trim().length > 0 || this.attachments().length > 0));
  readonly placeholder = computed(() => {
    if (this.disabled()) return 'Waiting for the agent connection…';
    if (this.prompting()) return 'Agent is thinking…';
    if (this.cancelling()) return 'Cancelling active turn…';
    return 'Type a message…';
  });

  constructor() {
    effect(() => {
      const locked = this.unavailable();
      if (locked && this.message.enabled) this.message.disable({ emitEvent: false });
      else if (!locked && this.message.disabled) this.message.enable({ emitEvent: false });
    });
    effect(() => {
      this.commandQuery();
      this.highlightedIndex.set(0);
    });
  }

  keyDown(event: KeyboardEvent): void {
    if (event.isComposing) return;
    if (event.ctrlKey && !event.metaKey && event.key.toLowerCase() === 'j') {
      event.preventDefault();
      this.insertNewline(event);
      return;
    }
    if (this.showCommands()) {
      if (event.key === 'ArrowDown') {
        event.preventDefault();
        this.moveHighlight(1);
        return;
      }
      if (event.key === 'ArrowUp') {
        event.preventDefault();
        this.moveHighlight(-1);
        return;
      }
      if (event.key === 'Tab') {
        event.preventDefault();
        this.completeHighlighted();
        return;
      }
      if (event.key === 'Enter' && !event.shiftKey) {
        event.preventDefault();
        this.completeHighlighted();
        return;
      }
      if (event.key === 'Escape') {
        event.preventDefault();
        this.dismissedQuery.set(this.commandQuery());
        return;
      }
    }
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      void this.send();
    }
  }

  private insertNewline(event: KeyboardEvent): void {
    const textarea = event.target as HTMLTextAreaElement | null;
    const current = this.message.value;
    const start = textarea?.selectionStart ?? current.length;
    const end = textarea?.selectionEnd ?? current.length;
    this.message.setValue(`${current.slice(0, start)}\n${current.slice(end)}`);
    const cursor = start + 1;
    queueMicrotask(() => {
      if (textarea) textarea.setSelectionRange(cursor, cursor);
    });
  }

  private moveHighlight(delta: number): void {
    const count = this.filteredCommands().length;
    if (!count) return;
    this.highlightedIndex.update((index) => (index + delta + count) % count);
    queueMicrotask(() => this.scrollHighlightedIntoView());
  }

  commandOptionId(index: number): string {
    return `${this.commandListId()}-option-${index}`;
  }

  private scrollHighlightedIntoView(): void {
    const list = this.commandList()?.nativeElement;
    if (!list) return;
    const index = Math.min(this.highlightedIndex(), this.filteredCommands().length - 1);
    const option = list.children[index] as HTMLElement | undefined;
    if (!option) return;

    const optionTop = option.offsetTop;
    const optionBottom = optionTop + option.offsetHeight;
    const visibleTop = list.scrollTop;
    const visibleBottom = visibleTop + list.clientHeight;
    if (optionTop < visibleTop) list.scrollTop = optionTop;
    else if (optionBottom > visibleBottom) list.scrollTop = optionBottom - list.clientHeight;
  }

  private completeHighlighted(): void {
    const command = this.highlightedCommand();
    if (command) this.completeCommand(command);
  }

  completeCommand(command: AvailableCommand): void {
    // Invoke a slash command as ordinary prompt text, never via a command RPC.
    // Insert only the command token so a hint never becomes a fake argument.
    this.message.setValue(`/${command.name} `);
    const textarea = this.messageInput()?.nativeElement;
    const end = this.message.value.length;
    queueMicrotask(() => {
      if (!textarea) return;
      textarea.focus();
      textarea.setSelectionRange(end, end);
    });
  }

  async send(): Promise<void> {
    const value = this.message.value.trim();
    if ((!value && !this.attachments().length) || !this.canSend()) return;
    const content = [...(value ? [{ type: 'text', text: value } satisfies RichContentBlock] : []), ...this.attachments()];
    this.message.setValue('');
    this.attachments.set([]);
    try {
      await this.state.sendPrompt(this.chatId(), content.length === 1 && content[0].type === 'text' ? value : content);
    } catch (error) {
      console.error('Failed to send prompt', error);
      this.message.setValue(value);
      this.attachments.set(content.filter((block) => block.type !== 'text'));
    }
  }

  chooseAttachment(input: HTMLInputElement): void {
    this.attachmentError.set(null);
    input.value = '';
    input.click();
  }

  async addAttachment(event: Event): Promise<void> {
    const file = (event.target as HTMLInputElement).files?.[0];
    if (!file) return;
    const type = file.type.toLowerCase();
    const kind = resolveAttachmentKind(type);
    if (!kind) {
      this.attachmentError.set('This file type is not supported');
      return;
    }
    const max = kind === 'resource' ? MAX_RESOURCE_BYTES : MAX_BINARY_BYTES;
    if (file.size > max) {
      this.attachmentError.set(`${kind === 'resource' ? 'Resource' : 'Attachment'} exceeds ${max / 1024 / 1024} MB`);
      return;
    }
    if (kind !== 'resource' && !(await this.matchesMime(file, type))) {
      this.attachmentError.set('The attachment data does not match its declared type');
      return;
    }
    const data = await this.fileBase64(file);
    const uri = `attachment://${encodeURIComponent(file.name)}`;
    const block: RichContentBlock = kind === 'image'
      ? { type: 'image', data, mimeType: type as Extract<RichContentBlock, { type: 'image' }>['mimeType'], uri }
      : kind === 'audio'
        ? { type: 'audio', data, mimeType: type as Extract<RichContentBlock, { type: 'audio' }>['mimeType'] }
        : { type: 'resource', resource: { text: await file.text(), uri, mimeType: type } };
    if (this.attachments().reduce((total, item) => total + this.blockBytes(item), 0) + file.size > MAX_TOTAL_BYTES) {
      this.attachmentError.set('Attachments together exceed 4 MB');
      return;
    }
    this.attachments.update((items) => [...items, block]);
  }

  removeAttachment(index: number): void { this.attachments.update((items) => items.filter((_, i) => i !== index)); }

  attachmentLabel(block: RichContentBlock): string {
    if (block.type === 'resource') return block.resource.uri.replace('attachment://', '');
    if (block.type === 'image') return block.uri?.replace('attachment://', '') ?? 'image attachment';
    return block.type === 'audio' ? 'audio attachment' : `${block.type} attachment`;
  }

  private blockBytes(block: RichContentBlock): number {
    if (block.type === 'image' || block.type === 'audio') return Math.floor(block.data.length * 0.75);
    if (block.type === 'resource') return 'text' in block.resource ? block.resource.text.length : Math.floor(block.resource.blob.length * 0.75);
    return 0;
  }

  private fileBase64(file: File): Promise<string> {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onerror = () => reject(reader.error);
      reader.onload = () => resolve(String(reader.result).split(',', 2)[1] ?? '');
      reader.readAsDataURL(file);
    });
  }


  async cancel(): Promise<void> {
    if (!this.chatId()) return;
    try {
      await this.state.cancelActiveTurn(this.chatId());
    } catch (error) {
      console.error('Failed to cancel turn', error);
    }
  }

  isGroup(value: ConfigOptionSelectValue | ConfigOptionSelectGroup): value is ConfigOptionSelectGroup {
    return 'options' in value;
  }

  optionLabel(option: ConfigOption): string {
    const currentValue = option.currentValue;
    for (const choice of option.options ?? []) {
      const values = this.isGroup(choice) ? choice.options : [choice];
      const selected = values.find((value) => Object.is(value.value, currentValue));
      if (selected) return selected.name;
    }
    return currentValue === undefined || currentValue === null ? option.name : String(currentValue);
  }

  async changeOption(option: ConfigOption, value: unknown): Promise<void> {
    if (!this.canConfigure()) return;
    try {
      await this.state.setChatConfig(this.chatId(), option.id, value);
    } catch (error) {
      console.error(`Failed to update ${option.name}`, error);
    }
  }

  private findOption(id: string, name: string, category?: string): ConfigOption | null {
    return (
      this.options().find(
        (option) =>
          option.type === 'select' &&
          (option.id === id ||
            option.name.toLowerCase() === name ||
            (category != null && option.category === category)),
      ) ?? null
    );
  }

  private async matchesMime(file: File, mime: string): Promise<boolean> {
    const bytes = new Uint8Array(await file.slice(0, 16).arrayBuffer());
    const starts = (...values: number[]) => values.every((value, index) => bytes[index] === value);
    if (mime === 'image/png') return starts(137, 80, 78, 71, 13, 10, 26, 10);
    if (mime === 'image/jpeg') return starts(255, 216, 255);
    if (mime === 'image/gif') return starts(71, 73, 70);
    if (mime === 'image/webp') return starts(82, 73, 70, 70) && startsAt(bytes, 8, 87, 69, 66, 80);
    if (mime === 'audio/mpeg') return starts(73, 68, 51) || bytes[0] === 255;
    if (mime === 'audio/wav') return starts(82, 73, 70, 70) && startsAt(bytes, 8, 87, 65, 86, 69);
    if (mime === 'audio/ogg') return starts(79, 103, 103, 83);
    return starts(0x1a, 0x45, 0xdf, 0xa3);
  }
}

function startsAt(bytes: Uint8Array, offset: number, ...values: number[]): boolean {
  return values.every((value, index) => bytes[offset + index] === value);
}
