import { ComponentFixture, TestBed } from '@angular/core/testing';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DisplayItem } from '../../core/api/types';
import { EventStreamComponent } from './event-stream';

interface ViewportState {
  scrollHeight: number;
  clientHeight: number;
  scrollTop: number;
}

/** Stubs the layout metrics jsdom never computes, so overflow/scroll math is testable. */
function mockViewport(element: HTMLElement, init: { scrollHeight: number; clientHeight: number; scrollTop?: number }): ViewportState {
  const state: ViewportState = { scrollHeight: init.scrollHeight, clientHeight: init.clientHeight, scrollTop: init.scrollTop ?? 0 };
  Object.defineProperties(element, {
    scrollHeight: { get: () => state.scrollHeight, configurable: true },
    clientHeight: { get: () => state.clientHeight, configurable: true },
    scrollTop: { get: () => state.scrollTop, set: (value: number) => { state.scrollTop = value; }, configurable: true },
  });
  (element as unknown as { scrollTo: (options: ScrollToOptions) => void }).scrollTo = (options: ScrollToOptions) => {
    if (typeof options.top === 'number') state.scrollTop = options.top;
  };
  return state;
}

const userMessage = (id: number, text: string): DisplayItem => ({ id, type: 'user_message', text, timestamp: '2026-01-01T00:00:00Z' });

describe('EventStreamComponent', () => {
  let fixture: ComponentFixture<EventStreamComponent>;

  // `detectChanges()` flushes any effect-scheduled `afterNextRender` callback
  // synchronously (this app is zoneless), so viewport metrics must be updated
  // *before* `detectChanges()` runs, not after.
  function create(inputs: {
    items?: DisplayItem[];
    chatId?: string;
    hasOlderHistory?: boolean;
    historyLoading?: boolean;
    historyError?: string;
    viewport?: { scrollHeight: number; clientHeight: number; scrollTop?: number };
  } = {}): { element: HTMLElement; state: ViewportState; requested: ReturnType<typeof vi.fn> } {
    fixture = TestBed.createComponent(EventStreamComponent);
    fixture.componentRef.setInput('items', inputs.items ?? []);
    fixture.componentRef.setInput('chatId', inputs.chatId ?? 'chat-1');
    fixture.componentRef.setInput('hasOlderHistory', inputs.hasOlderHistory ?? false);
    fixture.componentRef.setInput('historyLoading', inputs.historyLoading ?? false);
    fixture.componentRef.setInput('historyError', inputs.historyError ?? '');
    const requested = vi.fn();
    fixture.componentInstance.olderRequested.subscribe(requested);
    const element = fixture.nativeElement.querySelector('.stream') as HTMLElement;
    const state = mockViewport(element, inputs.viewport ?? { scrollHeight: 100, clientHeight: 400 });
    fixture.detectChanges();
    return { element, state, requested };
  }

  beforeEach(() => {
    TestBed.configureTestingModule({ imports: [EventStreamComponent] });
  });

  it('requests another page when the initial rendered page does not fill the viewport', async () => {
    const { requested } = create({ hasOlderHistory: true, viewport: { scrollHeight: 150, clientHeight: 400 } });
    await fixture.whenStable();

    expect(requested).toHaveBeenCalledTimes(1);
  });

  it('does not request another page when the initial page already overflows the viewport', async () => {
    const { requested } = create({ hasOlderHistory: true, viewport: { scrollHeight: 1000, clientHeight: 400 } });
    await fixture.whenStable();

    expect(requested).not.toHaveBeenCalled();
  });

  it('repeats initial-fill requests one page at a time until the viewport has useful overflow', async () => {
    const { requested, state } = create({ hasOlderHistory: true, viewport: { scrollHeight: 100, clientHeight: 400 } });
    await fixture.whenStable();
    expect(requested).toHaveBeenCalledTimes(1);

    // First page arrives, still undersized.
    fixture.componentRef.setInput('historyLoading', true);
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.componentRef.setInput('historyLoading', false);
    fixture.componentRef.setInput('items', [userMessage(1, 'a')]);
    state.scrollHeight = 250;
    fixture.detectChanges();
    await fixture.whenStable();
    expect(requested).toHaveBeenCalledTimes(2);

    // Second page arrives with enough content to overflow.
    fixture.componentRef.setInput('historyLoading', true);
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.componentRef.setInput('historyLoading', false);
    fixture.componentRef.setInput('items', [userMessage(1, 'a'), userMessage(2, 'b')]);
    state.scrollHeight = 650;
    fixture.detectChanges();
    await fixture.whenStable();
    expect(requested).toHaveBeenCalledTimes(2);

    // No further requests once satisfied, even across more render cycles.
    fixture.componentRef.setInput('items', [userMessage(1, 'a'), userMessage(2, 'b')]);
    fixture.detectChanges();
    await fixture.whenStable();
    expect(requested).toHaveBeenCalledTimes(2);
  });

  it('keeps initial fill pinned to the latest message instead of preserving the prior offset', async () => {
    const { requested, state, element } = create({ hasOlderHistory: true, viewport: { scrollHeight: 100, clientHeight: 400 } });
    await fixture.whenStable();
    expect(requested).toHaveBeenCalledTimes(1);

    fixture.componentRef.setInput('historyLoading', true);
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.componentRef.setInput('historyLoading', false);
    fixture.componentRef.setInput('items', [userMessage(1, 'older'), userMessage(2, 'newest')]);
    state.scrollHeight = 500;
    fixture.detectChanges();
    await fixture.whenStable();

    // A delta-preserving restore would have landed at top(0) + 500 - 100 = 400.
    expect(element.scrollTop).toBe(500);
  });

  it('automatically requests an older page once the user scrolls near the top', async () => {
    const { requested, state } = create({ hasOlderHistory: true, viewport: { scrollHeight: 1000, clientHeight: 400 } });
    await fixture.whenStable();
    expect(requested).not.toHaveBeenCalled();

    state.scrollTop = 100;
    fixture.componentInstance.onScroll();

    expect(requested).toHaveBeenCalledTimes(1);
  });

  it('does not request when scrollTop is still outside the prefetch threshold', async () => {
    const { requested, state } = create({ hasOlderHistory: true, viewport: { scrollHeight: 1000, clientHeight: 400 } });
    await fixture.whenStable();

    state.scrollTop = 900;
    fixture.componentInstance.onScroll();

    expect(requested).not.toHaveBeenCalled();
  });

  it('collapses repeated scroll events during a slow request into a single request', async () => {
    const { requested, state } = create({ hasOlderHistory: true, viewport: { scrollHeight: 1000, clientHeight: 400 } });
    await fixture.whenStable();

    state.scrollTop = 50;
    fixture.componentInstance.onScroll();
    fixture.componentInstance.onScroll();
    fixture.componentInstance.onScroll();
    fixture.componentInstance.onScroll();

    expect(requested).toHaveBeenCalledTimes(1);
  });

  it('preserves the visible viewport exactly when an older page is prepended', async () => {
    const { requested, state, element } = create({ hasOlderHistory: true, viewport: { scrollHeight: 1000, clientHeight: 400 } });
    await fixture.whenStable();

    // The user scrolled up to read older content, landing inside the prefetch zone.
    state.scrollTop = 50;
    fixture.componentInstance.onScroll();
    expect(requested).toHaveBeenCalledTimes(1);

    fixture.componentRef.setInput('historyLoading', true);
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.componentRef.setInput('historyLoading', false);
    fixture.componentRef.setInput('items', [userMessage(1, 'older'), userMessage(2, 'newest')]);
    state.scrollHeight = 1400;
    fixture.detectChanges();
    await fixture.whenStable();

    expect(element.scrollTop).toBe(50 + 1400 - 1000);
  });

  it('stops auto-loading once history is exhausted', async () => {
    const { requested } = create({ hasOlderHistory: false, viewport: { scrollHeight: 100, clientHeight: 400 } });
    await fixture.whenStable();

    expect(requested).not.toHaveBeenCalled();
  });

  it('stops mid-fill once the remaining page reports no older history', async () => {
    const { requested, state } = create({ hasOlderHistory: true, viewport: { scrollHeight: 100, clientHeight: 400 } });
    await fixture.whenStable();
    expect(requested).toHaveBeenCalledTimes(1);

    fixture.componentRef.setInput('historyLoading', true);
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.componentRef.setInput('historyLoading', false);
    fixture.componentRef.setInput('hasOlderHistory', false);
    fixture.componentRef.setInput('items', [userMessage(1, 'only page')]);
    state.scrollHeight = 150;
    fixture.detectChanges();
    await fixture.whenStable();

    expect(requested).toHaveBeenCalledTimes(1);
  });

  it('does not retry automatically after a failed request until the error clears', async () => {
    const { requested, state } = create({ hasOlderHistory: true, viewport: { scrollHeight: 100, clientHeight: 400 } });
    await fixture.whenStable();
    expect(requested).toHaveBeenCalledTimes(1);

    fixture.componentRef.setInput('historyLoading', true);
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.componentRef.setInput('historyLoading', false);
    fixture.componentRef.setInput('historyError', 'Failed to load chat history');
    fixture.detectChanges();
    await fixture.whenStable();
    expect(requested).toHaveBeenCalledTimes(1);

    // Further render cycles while the error persists must not retry.
    fixture.componentRef.setInput('items', []);
    fixture.detectChanges();
    await fixture.whenStable();
    expect(requested).toHaveBeenCalledTimes(1);

    // A later retry (through the manual button or an outside retry action) clears the error;
    // if it already filled the viewport, no further automatic request is needed.
    fixture.componentRef.setInput('historyError', '');
    fixture.componentRef.setInput('items', [userMessage(1, 'a'), userMessage(2, 'b')]);
    state.scrollHeight = 650;
    fixture.detectChanges();
    await fixture.whenStable();
    expect(requested).toHaveBeenCalledTimes(1);
  });

  it('resets local pagination/viewport-fill state when the chat id changes', async () => {
    const { requested, state } = create({ chatId: 'chat-1', hasOlderHistory: true, viewport: { scrollHeight: 1000, clientHeight: 400 } });
    await fixture.whenStable();
    expect(requested).not.toHaveBeenCalled();

    fixture.componentRef.setInput('chatId', 'chat-2');
    fixture.componentRef.setInput('items', []);
    fixture.componentRef.setInput('hasOlderHistory', true);
    state.scrollHeight = 100;
    fixture.detectChanges();
    await fixture.whenStable();

    expect(requested).toHaveBeenCalledTimes(1);
  });

  it('discards a pending preserved-scroll request when switching chats', async () => {
    const { requested, state, element } = create({
      chatId: 'chat-1',
      hasOlderHistory: true,
      viewport: { scrollHeight: 1000, clientHeight: 400 },
    });
    await fixture.whenStable();

    state.scrollTop = 50;
    fixture.componentInstance.onScroll();
    expect(requested).toHaveBeenCalledTimes(1);
    expect(fixture.componentInstance.showScrollButton()).toBe(true);

    fixture.componentRef.setInput('chatId', 'chat-2');
    fixture.componentRef.setInput('items', [userMessage(2, 'new chat latest message')]);
    fixture.componentRef.setInput('hasOlderHistory', false);
    state.scrollHeight = 600;
    fixture.detectChanges();
    await fixture.whenStable();

    expect(element.scrollTop).toBe(600);
    expect(fixture.componentInstance.showScrollButton()).toBe(false);
  });

  it('keeps streaming output auto-scrolled only while the user stays near the bottom', async () => {
    const { state, element } = create({ hasOlderHistory: false, viewport: { scrollHeight: 1000, clientHeight: 400 } });
    await fixture.whenStable();

    // User has scrolled away from the bottom to read older content.
    state.scrollTop = 0;
    fixture.componentInstance.onScroll();
    expect(element.scrollTop).toBe(0);

    fixture.componentRef.setInput('items', [userMessage(1, 'streamed chunk')]);
    state.scrollHeight = 1200;
    fixture.detectChanges();
    await fixture.whenStable();
    expect(element.scrollTop).toBe(0);

    // User returns to the bottom; the next streamed update auto-scrolls again.
    state.scrollTop = state.scrollHeight - 400;
    fixture.componentInstance.onScroll();
    fixture.componentRef.setInput('items', [userMessage(1, 'streamed chunk'), userMessage(2, 'more')]);
    state.scrollHeight = 1400;
    fixture.detectChanges();
    await fixture.whenStable();
    expect(element.scrollTop).toBe(1400);
  });
});
