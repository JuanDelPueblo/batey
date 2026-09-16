import { ComponentFixture, TestBed } from '@angular/core/testing';
import { describe, expect, it, beforeEach, vi } from 'vitest';
import { ChatComposerComponent } from './chat-composer';
import { AppStateService } from '../../state/app-state.service';
import type { AvailableCommand, ConfigOption, RichContentBlock } from '../../core/api/types';

describe('ChatComposerComponent', () => {
  let fixture: ComponentFixture<ChatComposerComponent>;
  let component: ChatComposerComponent;
  let resumed: string[] = [];
  const sendPrompt = async (_id: string, text: string | RichContentBlock[]) => sent.push(text);
  const connectChat = async (id: string) => { resumed.push(id); };
  const sent: Array<string | RichContentBlock[]> = [];

  beforeEach(async () => {
    sent.length = 0;
    resumed = [];
    await TestBed.configureTestingModule({
      imports: [ChatComposerComponent],
      providers: [
        {
          provide: AppStateService,
          useValue: { sendPrompt, connectChat, cancelActiveTurn: async () => undefined },
        },
      ],
    }).compileComponents();
    fixture = TestBed.createComponent(ChatComposerComponent);
    component = fixture.componentInstance;
    fixture.componentRef.setInput('chatId', 'chat-1');
    fixture.componentRef.setInput('turnState', 'IDLE');
    fixture.componentRef.setInput('disabled', false);
    fixture.detectChanges();
  });

  it('sends on plain Enter and clears the message', async () => {
    const textarea = fixture.nativeElement.querySelector('textarea') as HTMLTextAreaElement;
    textarea.value = '  hello agent  ';
    textarea.dispatchEvent(new Event('input', { bubbles: true }));
    const event = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true });
    textarea.dispatchEvent(event);
    await fixture.whenStable();
    expect(event.defaultPrevented).toBe(true);
    expect(sent).toEqual(['hello agent']);
    expect(component.message.value).toBe('');
  });

  it('does not send on Shift+Enter', async () => {
    const textarea = fixture.nativeElement.querySelector('textarea') as HTMLTextAreaElement;
    textarea.value = 'line1';
    textarea.dispatchEvent(new Event('input', { bubbles: true }));
    const event = new KeyboardEvent('keydown', { key: 'Enter', shiftKey: true, bubbles: true, cancelable: true });
    textarea.dispatchEvent(event);
    await fixture.whenStable();
    expect(event.defaultPrevented).toBe(false);
    expect(sent).toEqual([]);
    expect(component.message.value).toBe('line1');
  });

  it('inserts a newline at the cursor on Ctrl+J without sending', async () => {
    const textarea = fixture.nativeElement.querySelector('textarea') as HTMLTextAreaElement;
    textarea.value = 'hello world';
    textarea.dispatchEvent(new Event('input', { bubbles: true }));
    textarea.setSelectionRange(5, 5);
    const event = new KeyboardEvent('keydown', { key: 'j', ctrlKey: true, bubbles: true, cancelable: true });
    textarea.dispatchEvent(event);
    await fixture.whenStable();
    expect(event.defaultPrevented).toBe(true);
    expect(sent).toEqual([]);
    expect(component.message.value).toBe('hello\n world');
    expect(textarea.selectionStart).toBe(6);
    expect(textarea.selectionEnd).toBe(6);
  });

  it('sends through Ctrl+Enter and clears the Material form control', async () => {
    const textarea = fixture.nativeElement.querySelector('textarea') as HTMLTextAreaElement;
    expect(textarea.disabled).toBe(false);
    textarea.value = '  hello agent  ';
    textarea.dispatchEvent(new Event('input', { bubbles: true }));
    textarea.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', ctrlKey: true, bubbles: true }));
    await fixture.whenStable();
    expect(sent).toEqual(['hello agent']);
    expect(component.message.value).toBe('');
  });

  it('a stopped chat can send normally without an explicit connection action', async () => {
    const textarea = fixture.nativeElement.querySelector('textarea') as HTMLTextAreaElement;
    expect(textarea.disabled).toBe(false);
    expect(fixture.nativeElement.querySelector('.resume-button')).toBeNull();

    textarea.value = 'hello from stopped chat';
    textarea.dispatchEvent(new Event('input', { bubbles: true }));
    await component.send();
    await fixture.whenStable();

    expect(sent).toEqual(['hello from stopped chat']);
    expect(resumed).toHaveLength(0);
  });

  it('disables the textarea while the agent is not ready', () => {
    fixture.componentRef.setInput('disabled', true);
    fixture.detectChanges();
    expect((fixture.nativeElement.querySelector('textarea') as HTMLTextAreaElement).disabled).toBe(true);
  });


  it('places model and reasoning selectors in the composer footer', () => {
    const options: ConfigOption[] = [
      {
        id: 'model',
        name: 'Model',
        type: 'select',
        currentValue: 'gpt-5',
        options: [{ value: 'gpt-5', name: 'GPT-5' }],
      },
      {
        id: 'reasoning_effort',
        name: 'Reasoning effort',
        type: 'select',
        currentValue: 'medium',
        options: [{ value: 'medium', name: 'Medium' }],
      },
    ];
    fixture.componentRef.setInput('options', options);
    fixture.detectChanges();

    const selectors = fixture.nativeElement.querySelectorAll('.selector');
    expect(selectors).toHaveLength(2);
    expect(selectors[0].textContent).toContain('GPT-5');
    expect(selectors[1].textContent).toContain('Reasoning effort: Medium');
  });

  it('restores message text when sending prompt fails', async () => {
    const state = TestBed.inject(AppStateService);
    vi.spyOn(state, 'sendPrompt').mockRejectedValueOnce(new Error('Agent busy'));
    const textarea = fixture.nativeElement.querySelector('textarea') as HTMLTextAreaElement;
    textarea.value = 'failed prompt text';
    textarea.dispatchEvent(new Event('input', { bubbles: true }));
    expect(component.message.value).toBe('failed prompt text');

    await component.send();
    fixture.detectChanges();

    expect(component.message.value).toBe('failed prompt text');
    expect(textarea.value).toBe('failed prompt text');
  });

  it('accepts image, audio, and text attachments from one file picker', async () => {
    const input = document.createElement('input');
    vi.spyOn(input, 'click');
    component.chooseAttachment(input);
    expect(input.click).toHaveBeenCalled();
    expect(component.attachmentAccept).toContain('image/png');
    expect(component.attachmentAccept).toContain('audio/mpeg');
    expect(component.attachmentAccept).toContain('text/markdown');

    const png = new File([new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10])], 'pic.png', { type: 'image/png' });
    Object.defineProperty(input, 'files', { value: [png], configurable: true });
    await component.addAttachment({ target: input } as unknown as Event);
    expect(component.attachments()[0]).toEqual(expect.objectContaining({ type: 'image', mimeType: 'image/png' }));

    const mp3 = new File([new Uint8Array([73, 68, 51])], 'note.mp3', { type: 'audio/mpeg' });
    Object.defineProperty(input, 'files', { value: [mp3], configurable: true });
    await component.addAttachment({ target: input } as unknown as Event);
    expect(component.attachments()[1]).toEqual(expect.objectContaining({ type: 'audio', mimeType: 'audio/mpeg' }));

    const md = new File(['# Title'], 'notes.md', { type: 'text/markdown' });
    Object.defineProperty(input, 'files', { value: [md], configurable: true });
    await component.addAttachment({ target: input } as unknown as Event);
    expect(component.attachments()[2]).toEqual(expect.objectContaining({ type: 'resource' }));
    expect(component.attachmentError()).toBeNull();
  });

  it('reports one clear error for unsupported files', async () => {
    const input = document.createElement('input');
    const executable = new File(['not an image'], 'bad.exe', { type: 'application/octet-stream' });
    Object.defineProperty(input, 'files', { value: [executable], configurable: true });
    await component.addAttachment({ target: input } as unknown as Event);
    expect(component.attachmentError()).toBe('This file type is not supported');
    expect(component.attachments()).toHaveLength(0);
  });

  it('rejects binary data that does not match its declared type', async () => {
    const input = document.createElement('input');
    const fake = new File(['not really a png'], 'fake.png', { type: 'image/png' });
    Object.defineProperty(input, 'files', { value: [fake], configurable: true });
    await component.addAttachment({ target: input } as unknown as Event);
    expect(component.attachmentError()).toBe('The attachment data does not match its declared type');
    expect(component.attachments()).toHaveLength(0);
  });

  it('keeps the resource and binary size limits', async () => {
    const input = document.createElement('input');
    const bigResource = new File([new Uint8Array(600 * 1024)], 'big.txt', { type: 'text/plain' });
    Object.defineProperty(input, 'files', { value: [bigResource], configurable: true });
    await component.addAttachment({ target: input } as unknown as Event);
    expect(component.attachmentError()).toBe('Resource exceeds 0.5 MB');
    expect(component.attachments()).toHaveLength(0);

    const bigImage = new File([new Uint8Array(3 * 1024 * 1024)], 'big.png', { type: 'image/png' });
    Object.defineProperty(input, 'files', { value: [bigImage], configurable: true });
    await component.addAttachment({ target: input } as unknown as Event);
    expect(component.attachmentError()).toBe('Attachment exceeds 2 MB');
    expect(component.attachments()).toHaveLength(0);
  });

  function setCommands(commands: AvailableCommand[] = [
      { name: 'help', description: 'Show help', input: { hint: 'topic' } },
      { name: 'clear', description: 'Clear the chat' },
    ]): void {
    fixture.componentRef.setInput('commands', commands);
    fixture.detectChanges();
  }

  function type(value: string): void {
    const textarea = fixture.nativeElement.querySelector('textarea') as HTMLTextAreaElement;
    textarea.value = value;
    textarea.dispatchEvent(new Event('input', { bubbles: true }));
    fixture.detectChanges();
  }

  function press(key: string, init: KeyboardEventInit = {}): KeyboardEvent {
    const textarea = fixture.nativeElement.querySelector('textarea') as HTMLTextAreaElement;
    const event = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true, ...init });
    textarea.dispatchEvent(event);
    fixture.detectChanges();
    return event;
  }

  function options(): NodeListOf<HTMLButtonElement> {
    return fixture.nativeElement.querySelectorAll('.command-option');
  }

  it('renders an autocomplete menu and filters commands as the user types', () => {
    setCommands();
    type('/');
    expect(options()).toHaveLength(2);
    const first = options()[0];
    expect(first.textContent).toContain('/help');
    expect(first.textContent).toContain('Show help');
    expect(first.textContent).toContain('topic');

    type('/he');
    expect(options()).toHaveLength(1);
    expect(options()[0].textContent).toContain('/help');
  });

  it('shows every matching command and links the textarea to its active option', () => {
    setCommands(commandCatalog(15));
    type('/');

    expect(options()).toHaveLength(15);
    expect(options()[6].textContent).toContain('/command-07');
    expect(options()[14].textContent).toContain('/command-15');

    const textarea = fixture.nativeElement.querySelector('textarea') as HTMLTextAreaElement;
    const list = fixture.nativeElement.querySelector('.command-list') as HTMLElement;
    expect(textarea.getAttribute('aria-controls')).toBe(list.id);
    expect(textarea.getAttribute('aria-activedescendant')).toBe(options()[0].id);
  });

  it('moves the highlighted suggestion with arrow keys', () => {
    setCommands();
    type('/');
    expect(component.highlightedCommand()?.name).toBe('help');
    press('ArrowDown');
    expect(component.highlightedCommand()?.name).toBe('clear');
    press('ArrowUp');
    expect(component.highlightedCommand()?.name).toBe('help');
  });

  it('navigates across the old six-command boundary and wraps around the full list', () => {
    setCommands(commandCatalog(15));
    type('/');

    for (let index = 0; index < 7; index += 1) press('ArrowDown');
    expect(component.highlightedCommand()?.name).toBe('command-08');

    for (let index = 0; index < 8; index += 1) press('ArrowDown');
    expect(component.highlightedCommand()?.name).toBe('command-01');

    press('ArrowUp');
    expect(component.highlightedCommand()?.name).toBe('command-15');
  });

  it('resets the highlight when the slash-command filter changes', () => {
    setCommands(commandCatalog(15));
    type('/command-0');
    for (let index = 0; index < 7; index += 1) press('ArrowDown');
    expect(component.highlightedCommand()?.name).toBe('command-08');

    type('/command-01');
    expect(component.highlightedCommand()?.name).toBe('command-01');
  });

  it('scrolls the command list by the minimum amount needed for the highlighted option', async () => {
    setCommands(commandCatalog(15));
    type('/');
    const list = fixture.nativeElement.querySelector('.command-list') as HTMLElement;
    Object.defineProperties(list, {
      clientHeight: { value: 120, configurable: true },
      scrollTop: { value: 0, writable: true, configurable: true },
    });
    Array.from(options()).forEach((option, index) => {
      Object.defineProperties(option, {
        offsetTop: { value: index * 40, configurable: true },
        offsetHeight: { value: 40, configurable: true },
      });
    });

    for (let index = 0; index < 7; index += 1) press('ArrowDown');
    await Promise.resolve();

    expect(component.highlightedCommand()?.name).toBe('command-08');
    expect(list.scrollTop).toBe(200);
  });

  it('completes the highlighted command on Tab without inserting the hint or sending', () => {
    setCommands();
    type('/he');
    const event = press('Tab');
    expect(event.defaultPrevented).toBe(true);
    expect(component.message.value).toBe('/help ');
    expect(component.message.value).not.toContain('topic');
    expect(sent).toEqual([]);
  });

  it('completes the highlighted command on Enter rather than sending', () => {
    setCommands();
    type('/');
    press('ArrowDown');
    const event = press('Enter');
    expect(event.defaultPrevented).toBe(true);
    expect(component.message.value).toBe('/clear ');
    expect(sent).toEqual([]);
  });

  it('completes a command on mouse click without sending', () => {
    setCommands();
    type('/');
    options()[1].click();
    fixture.detectChanges();
    expect(component.message.value).toBe('/clear ');
    expect(sent).toEqual([]);
  });

  it('closes suggestions on Escape and keeps the typed query', () => {
    setCommands();
    type('/he');
    press('Escape');
    expect(options()).toHaveLength(0);
    expect(component.message.value).toBe('/he');
  });

  it('sends the completed command as ordinary prompt text on the next Enter', async () => {
    setCommands();
    type('/he');
    press('Tab');
    const event = press('Enter');
    await fixture.whenStable();
    expect(event.defaultPrevented).toBe(true);
    expect(sent).toEqual(['/help']);
  });

  function commandCatalog(count: number): AvailableCommand[] {
    return Array.from({ length: count }, (_, index) => ({
      name: `command-${String(index + 1).padStart(2, '0')}`,
      description: `Command ${index + 1}`,
      input: { hint: 'argument' },
    }));
  }
});
