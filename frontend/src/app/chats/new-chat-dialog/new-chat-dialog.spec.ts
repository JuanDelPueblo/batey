import { ComponentFixture, TestBed } from '@angular/core/testing';
import { MAT_DIALOG_DATA, MatDialogRef } from '@angular/material/dialog';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ApiService } from '../../core/api/api.service';
import { AppStateService } from '../../state/app-state.service';
import { NewChatDialogComponent } from './new-chat-dialog';

describe('NewChatDialogComponent', () => {
  let fixture: ComponentFixture<NewChatDialogComponent>;
  let state: { agents: ReturnType<typeof vi.fn>; createChat: ReturnType<typeof vi.fn> };
  let api: {
    fetchWorkspaceOptions: ReturnType<typeof vi.fn>;
    syncWorkspace: ReturnType<typeof vi.fn>;
  };
  let close: ReturnType<typeof vi.fn>;

  async function setup(options: unknown, agents = [
    { id: 'codex', display_name: 'Codex CLI', source: 'builtin', availability: 'available', metadata: null },
    { id: 'claude', display_name: 'Claude', source: 'file', availability: 'available', metadata: null },
  ]) {
    state = { agents: vi.fn(() => agents), createChat: vi.fn(async () => ({ id: 'chat-1' })) };
    api = {
      fetchWorkspaceOptions: vi.fn(async () => options),
      syncWorkspace: vi.fn(),
    };
    close = vi.fn();
    await TestBed.configureTestingModule({
      imports: [NewChatDialogComponent],
      providers: [
        { provide: ApiService, useValue: api },
        { provide: AppStateService, useValue: state },
        { provide: MAT_DIALOG_DATA, useValue: { projectId: 'project-1' } },
        { provide: MatDialogRef, useValue: { close } },
      ],
    }).compileComponents();
    fixture = TestBed.createComponent(NewChatDialogComponent);
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.detectChanges();
  }

  beforeEach(() => TestBed.resetTestingModule());

  it('defaults Git chats to isolated mode and the current branch, and shows dirty guidance', async () => {
    await setup({ is_git: true, current_branch: 'main', head_sha: 'a'.repeat(40), dirty: true,
      branches: [{ name: 'main', sha: 'a'.repeat(40), current: true }, { name: 'feature', sha: 'b'.repeat(40), current: false }] });
    expect(fixture.componentInstance.selectedMode()).toBe('managed_worktree');
    expect(fixture.componentInstance.selectedBranch()).toBe('main');
    expect(fixture.nativeElement.textContent).toContain('not included');
    fixture.componentInstance.selectedMode.set('project_checkout');
    fixture.detectChanges();
    await fixture.componentInstance.create();
    expect(state.createChat).toHaveBeenCalledWith('project-1', 'codex', undefined, {
      mode: 'project_checkout', branch: 'main',
    });
  });

  it('keeps non-Git creation to agent selection', async () => {
    await setup({ is_git: false, current_branch: null, head_sha: null, dirty: false, branches: [] });
    expect(fixture.nativeElement.textContent).not.toContain('Workspace mode');
    expect(fixture.nativeElement.textContent).not.toContain('Branch');
    expect(fixture.nativeElement.textContent).not.toContain('Update from remote');
    await fixture.componentInstance.create();
    expect(state.createChat).toHaveBeenCalledWith('project-1', 'codex', undefined, undefined);
  });

  it('updates from remote, reports the result, and reloads workspace options while preserving selections', async () => {
    const initial = {
      is_git: true, current_branch: 'main', head_sha: 'a'.repeat(40), dirty: false,
      branches: [{ name: 'main', sha: 'a'.repeat(40), current: true }, { name: 'feature', sha: 'b'.repeat(40), current: false }],
    };
    await setup(initial);
    fixture.componentInstance.selectedMode.set('project_checkout');
    fixture.componentInstance.selectedBranch.set('feature');

    const updated = {
      is_git: true, current_branch: 'main', head_sha: 'c'.repeat(40), dirty: false,
      branches: [{ name: 'main', sha: 'c'.repeat(40), current: true }, { name: 'feature', sha: 'b'.repeat(40), current: false }],
    };
    api.syncWorkspace.mockResolvedValue({ branch: 'main', remote: 'origin', updated: true, head_sha: 'c'.repeat(40) });
    api.fetchWorkspaceOptions.mockResolvedValue(updated);

    const syncPromise = fixture.componentInstance.updateFromRemote();
    expect(fixture.componentInstance.syncing()).toBe(true);
    await syncPromise;

    expect(api.syncWorkspace).toHaveBeenCalledWith('project-1');
    expect(fixture.componentInstance.syncing()).toBe(false);
    expect(fixture.componentInstance.syncMessage()).toBe('Updated main');
    expect(fixture.componentInstance.options()).toEqual(updated);
    // Selections the user made survive the post-sync reload.
    expect(fixture.componentInstance.selectedMode()).toBe('project_checkout');
    expect(fixture.componentInstance.selectedBranch()).toBe('feature');
  });

  it('reports "Already up to date" without changing the selected branch', async () => {
    await setup({
      is_git: true, current_branch: 'main', head_sha: 'a'.repeat(40), dirty: false,
      branches: [{ name: 'main', sha: 'a'.repeat(40), current: true }],
    });
    api.syncWorkspace.mockResolvedValue({ branch: 'main', remote: 'origin', updated: false, head_sha: 'a'.repeat(40) });

    await fixture.componentInstance.updateFromRemote();

    expect(fixture.componentInstance.syncMessage()).toBe('Already up to date');
  });

  it('surfaces a sync failure without closing the dialog or losing selections', async () => {
    await setup({
      is_git: true, current_branch: 'main', head_sha: 'a'.repeat(40), dirty: true,
      branches: [{ name: 'main', sha: 'a'.repeat(40), current: true }],
    });
    fixture.componentInstance.selectedMode.set('project_checkout');
    const error = Object.assign(new Error('Cannot update checkout with uncommitted changes'), { name: 'ApiError' });
    api.syncWorkspace.mockRejectedValue(error);

    await fixture.componentInstance.updateFromRemote();

    expect(fixture.componentInstance.errorMessage()).toBe('Cannot update checkout with uncommitted changes');
    expect(fixture.componentInstance.syncMessage()).toBe('');
    expect(fixture.componentInstance.selectedMode()).toBe('project_checkout');
    expect(close).not.toHaveBeenCalled();
    // fetchWorkspaceOptions is not re-invoked past the initial load on failure.
    expect(api.fetchWorkspaceOptions).toHaveBeenCalledTimes(1);
  });

  it('ignores a duplicate update-from-remote click while one is already running', async () => {
    await setup({
      is_git: true, current_branch: 'main', head_sha: 'a'.repeat(40), dirty: false,
      branches: [{ name: 'main', sha: 'a'.repeat(40), current: true }],
    });
    let resolveSync!: (value: unknown) => void;
    api.syncWorkspace.mockReturnValue(new Promise((resolve) => { resolveSync = resolve; }));

    const first = fixture.componentInstance.updateFromRemote();
    const second = fixture.componentInstance.updateFromRemote();
    resolveSync({ branch: 'main', remote: 'origin', updated: false, head_sha: 'a'.repeat(40) });
    await Promise.all([first, second]);

    expect(api.syncWorkspace).toHaveBeenCalledTimes(1);
  });

  it('loads the first available catalog entry and disables unavailable entries', async () => {
    await setup({ is_git: false, current_branch: null, head_sha: null, dirty: false, branches: [] }, [
      { id: 'offline', display_name: 'Offline', source: 'file', availability: 'unavailable', metadata: null },
      { id: 'custom', display_name: 'Custom ACP', source: 'batey_managed', availability: 'available', metadata: null },
    ]);
    expect(fixture.componentInstance.selectedAgent()).toBe('custom');
    expect(fixture.componentInstance.availableAgents().map((agent) => agent.id)).toEqual(['custom']);
  });
});
