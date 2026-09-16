import { Component, computed, inject, output } from '@angular/core';
import { MatButtonModule } from '@angular/material/button';
import { MatDividerModule } from '@angular/material/divider';
import { MatIconModule } from '@angular/material/icon';
import { MatListModule } from '@angular/material/list';
import { MatMenuModule } from '@angular/material/menu';
import { MatTooltipModule } from '@angular/material/tooltip';
import { MatDialog, MatDialogModule } from '@angular/material/dialog';
import { Router, RouterLink, RouterLinkActive } from '@angular/router';
import type { Chat } from '../../core/api/types';
import { AppStateService } from '../../state/app-state.service';
import { chatActivityLabel, type ChatActivity } from '../../state/chat-activity';
import { compareChatsByRecency } from '../../state/chat-session.store';
import { NewChatButtonComponent } from '../../chats/new-chat-button/new-chat-button';
import { ChatStatusBadgeComponent } from '../../shared/chat-status-badge/chat-status-badge';
import { DeleteChatDialogComponent } from '../../chats/delete-chat-dialog/delete-chat-dialog';
import { RenameChatDialogComponent } from '../../chats/rename-chat-dialog/rename-chat-dialog';
import { ProjectDialogComponent } from '../../projects/project-dialog/project-dialog';
import { ConnectionStatusComponent } from '../connection-status/connection-status';
import { ThemeService } from '../../core/theme.service';
import { ActivityClockService } from '../../shared/chat-status-badge/activity-clock.service';
import { APP_VERSION } from '../../version';

/**
 * The navigation drawer for a selected project. It holds the project switcher
 * and the chats of the active project. The shell hides it when no project is
 * selected.
 */
@Component({
  selector: 'hub-navigation',
  imports: [
    ChatStatusBadgeComponent,
    ConnectionStatusComponent,
    MatButtonModule,
    MatDialogModule,
    MatDividerModule,
    MatIconModule,
    MatListModule,
    MatMenuModule,
    MatTooltipModule,
    NewChatButtonComponent,
    RouterLink,
    RouterLinkActive,
  ],
  templateUrl: './navigation.html',
  styleUrl: './navigation.scss',
})
export class NavigationComponent {
  readonly closeRequested = output<void>();
  readonly appVersion = APP_VERSION;
  readonly state = inject(AppStateService);
  readonly theme = inject(ThemeService);
  private readonly dialog = inject(MatDialog);
  private readonly router = inject(Router);
  private readonly clock = inject(ActivityClockService);

  /** The project overview page carries its own button, so the drawer hides one. */
  readonly showNewChat = computed(() => this.state.activeChatId() !== null);

  readonly visibleChats = computed(() => {
    const projectId = this.state.activeProjectId();
    const chats = projectId ? this.state.chatsByProject()[projectId] ?? [] : [];
    return [...(this.state.showArchived() ? chats : chats.filter((chat) => !chat.archived))]
      .sort(compareChatsByRecency);
  });

  agentLabel(agent: string | null | undefined): string {
    const value = agent ?? '';
    if (!value) return value;
    return value.charAt(0).toUpperCase() + value.slice(1).toLowerCase();
  }

  activityFor(chat: Chat): ChatActivity {
    return this.state.chatActivity(chat.id);
  }

  chatAriaLabel(chat: Chat): string {
    return `Open chat ${chat.title || 'Untitled chat'}, ${chatActivityLabel(
      this.activityFor(chat),
      this.state.chatTurnStartedAt(chat.id),
      this.clock.now(),
    )}`;
  }

  turnStartedAtFor(chat: Chat): string | null {
    return this.state.chatTurnStartedAt(chat.id);
  }

  /**
   * Opens the chosen project. Inside a chat view the project switches in
   * place: the workspace shows the latest chat of the new project. A project
   * without chats opens its project page instead.
   */
  async openProject(projectId: string): Promise<void> {
    if (projectId === this.state.activeProjectId()) {
      this.closeRequested.emit();
      return;
    }
    const chat = this.state.activeChatId() ? await this.latestVisibleChat(projectId) : null;
    void this.router.navigate(
      chat ? ['/projects', projectId, 'chats', chat.id] : ['/projects', projectId],
    );
    this.closeRequested.emit();
  }

  /** Returns the latest visible chat of the project, and loads the list on demand. */
  private async latestVisibleChat(projectId: string): Promise<Chat | null> {
    let chats = this.state.chatsByProject()[projectId];
    if (!chats) {
      await this.state.loadChats(projectId);
      chats = this.state.chatsByProject()[projectId];
    }
    if (!chats) return null;
    const visible = this.state.showArchived()
      ? chats
      : chats.filter((candidate) => !candidate.archived);
    return [...visible].sort(compareChatsByRecency)[0] ?? null;
  }

  goHome(): void {
    void this.router.navigate(['/']);
    this.closeRequested.emit();
  }

  newProject(): void {
    this.dialog.open(ProjectDialogComponent, { width: 'min(720px, calc(100vw - 32px))', panelClass: 'hub-wide-dialog' });
    this.closeRequested.emit();
  }

  renameChat(chat: Chat): void {
    this.dialog.open(RenameChatDialogComponent, {
      width: 'min(480px, calc(100vw - 32px))',
      data: chat,
    });
  }

  async archiveChat(chat: Chat): Promise<void> {
    await this.state.archiveChat(chat.id, !chat.archived)
      .catch((error) => console.error('Failed to archive chat', error));
  }

  removeChat(chat: Chat): void {
    this.dialog.open(DeleteChatDialogComponent, {
      width: 'min(520px, calc(100vw - 32px))',
      data: chat,
    });
  }
}
