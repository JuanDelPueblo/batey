import {
  Component,
  ElementRef,
  afterNextRender,
  effect,
  inject,
  input,
  output,
  signal,
  viewChild,
} from '@angular/core';
import { Injector } from '@angular/core';
import { MatButtonModule } from '@angular/material/button';
import { MatIconModule } from '@angular/material/icon';
import { MatTooltipModule } from '@angular/material/tooltip';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import type { DisplayItem } from '../../core/api/types';
import { MessageItemComponent } from '../message-item/message-item';

@Component({
  selector: 'hub-event-stream',
  imports: [MatButtonModule, MatIconModule, MatProgressSpinnerModule, MatTooltipModule, MessageItemComponent],
  templateUrl: './event-stream.html',
  styleUrl: './event-stream.scss',
})
export class EventStreamComponent {
  /** Minimum scrollable overflow, in pixels, that counts as a useful scrollable viewport. */
  private static readonly MIN_OVERFLOW_PX = 200;
  /** Distance from the top, in pixels, that triggers an automatic older-page prefetch. */
  private static readonly PREFETCH_THRESHOLD_PX = 400;

  readonly items = input<DisplayItem[]>([]);
  readonly chatId = input('');
  readonly showScrollButton = signal(false);
  readonly hasOlderHistory = input(false);
  readonly historyLoading = input(false);
  readonly historyError = input('');
  readonly olderRequested = output<void>();

  private readonly viewport = viewChild<ElementRef<HTMLElement>>('viewport');
  private readonly injector = inject(Injector);
  private autoScroll = true;
  private preserveScroll: { top: number; height: number } | null = null;
  private viewportChatId: string | null = null;

  /** Tracks the chat the fill/prefetch state below belongs to, so a chat switch resets it. */
  private fillChatId: string | null = null;
  /** True once the initial page(s) give the viewport useful overflow, or history is exhausted. */
  private initialFillSatisfied = false;
  /** True from the moment this component emits an automatic request until it settles. */
  private fillRequestPending = false;

  constructor() {
    // The item list gets a new identity on every streamed chunk, so this reacts
    // to appended text inside an open turn as well as to a new item.
    effect(() => {
      const chatId = this.chatId();
      if (chatId !== this.viewportChatId) {
        this.viewportChatId = chatId;
        this.preserveScroll = null;
        this.autoScroll = true;
        this.showScrollButton.set(false);
      }
      this.items();
      if (this.historyLoading()) return;
      const preserve = this.preserveScroll;
      this.preserveScroll = null;
      if (preserve) {
        afterNextRender({ read: () => {
          const element = this.viewport()?.nativeElement;
          if (!element) return;
          element.scrollTop = preserve.top + element.scrollHeight - preserve.height;
        } }, { injector: this.injector });
        return;
      }
      if (!this.autoScroll) return;
      afterNextRender({ read: () => this.scrollToBottom() }, { injector: this.injector });
    });

    // Fills an undersized initial viewport one page at a time, and re-checks the
    // near-top prefetch zone once fill is done and a page finishes rendering.
    effect(() => {
      const chatId = this.chatId();
      this.items();
      const loading = this.historyLoading();
      const hasOlder = this.hasOlderHistory();

      if (chatId !== this.fillChatId) {
        this.fillChatId = chatId;
        this.initialFillSatisfied = false;
        this.fillRequestPending = false;
        this.preserveScroll = null;
        this.autoScroll = true;
        this.showScrollButton.set(false);
      }

      if (loading) {
        this.fillRequestPending = true;
        return;
      }
      this.fillRequestPending = false;

      if (!hasOlder) {
        this.initialFillSatisfied = true;
        return;
      }

      afterNextRender({ read: () => this.evaluateFill() }, { injector: this.injector });
    });
  }

  private evaluateFill(): void {
    if (this.fillRequestPending) return;
    const element = this.viewport()?.nativeElement;
    if (!element) return;

    if (!this.initialFillSatisfied) {
      const overflow = element.scrollHeight - element.clientHeight;
      if (overflow < EventStreamComponent.MIN_OVERFLOW_PX) {
        this.requestFill();
        return;
      }
      this.initialFillSatisfied = true;
    }

    this.maybePrefetchNearTop(element);
  }

  /** Requests another page to fill an undersized viewport, staying pinned to the newest message. */
  private requestFill(): void {
    if (this.fillRequestPending || this.historyLoading() || !this.hasOlderHistory() || this.historyError()) return;
    this.fillRequestPending = true;
    this.olderRequested.emit();
  }

  /** Requests another page when the viewport is within the near-top prefetch zone. */
  private maybePrefetchNearTop(element: HTMLElement): void {
    if (this.fillRequestPending || this.historyLoading() || !this.hasOlderHistory() || this.historyError()) return;
    if (element.scrollTop > EventStreamComponent.PREFETCH_THRESHOLD_PX) return;
    this.fillRequestPending = true;
    this.preserveScroll = { top: element.scrollTop, height: element.scrollHeight };
    this.olderRequested.emit();
  }

  requestOlder(): void {
    const element = this.viewport()?.nativeElement;
    if (this.historyLoading() || !this.hasOlderHistory()) return;
    if (element) this.preserveScroll = { top: element.scrollTop, height: element.scrollHeight };
    this.fillRequestPending = true;
    this.olderRequested.emit();
  }

  onScroll(): void {
    const element = this.viewport()?.nativeElement;
    if (!element) return;
    const distance = element.scrollHeight - element.scrollTop - element.clientHeight;
    this.autoScroll = distance <= 80;
    this.showScrollButton.set(distance > 200);
    if (this.initialFillSatisfied) this.maybePrefetchNearTop(element);
  }

  scrollToBottom(smooth = false): void {
    const element = this.viewport()?.nativeElement;
    if (!element) return;
    element.scrollTo({ top: element.scrollHeight, behavior: smooth ? 'smooth' : 'auto' });
    this.autoScroll = true;
    this.showScrollButton.set(false);
  }
}
