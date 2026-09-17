import { ComponentFixture, TestBed } from '@angular/core/testing';
import { signal } from '@angular/core';
import { MatDialog } from '@angular/material/dialog';
import { of } from 'rxjs';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { AgentOperation, RegistryCatalog } from '../../core/api/types';
import { AppStateService } from '../../state/app-state.service';
import { RegistryBrowserComponent } from './registry-browser';

const catalog: RegistryCatalog = {
  status: 'cached',
  source_url: 'https://registry.example.invalid',
  host: 'linux-x86_64',
  rejected: [],
  agents: [
    {
      id: 'example-acp',
      name: 'Example ACP',
      version: '1.2.0',
      description: 'An example.',
      distributions: ['npx'],
      platforms: [],
      installed_as: 'example-acp',
      installed_version: '1.0.0',
      update_available: true,
    },
    {
      id: 'native-agent',
      name: 'Native Agent',
      version: '2.0.0',
      description: 'A native agent.',
      distributions: ['binary'],
      platforms: ['linux-x86_64'],
      selected_distribution: 'binary',
      update_available: false,
    },
    {
      id: 'windows-only',
      name: 'Windows Only',
      version: '1.0.0',
      description: 'Not for this host.',
      distributions: ['binary'],
      platforms: ['windows-x86_64'],
      unsupported_reason: 'No binary distribution covers this platform.',
      update_available: false,
    },
  ],
};

describe('RegistryBrowserComponent', () => {
  let fixture: ComponentFixture<RegistryBrowserComponent>;
  let state: {
    registry: ReturnType<typeof signal<RegistryCatalog | null>>;
    registryLoading: ReturnType<typeof signal<boolean>>;
    registryError: ReturnType<typeof signal<string | null>>;
    loadRegistry: ReturnType<typeof vi.fn>;
    refreshRegistry: ReturnType<typeof vi.fn>;
    installRegistryAgent: ReturnType<typeof vi.fn>;
    updateAgent: ReturnType<typeof vi.fn>;
    removeAgent: ReturnType<typeof vi.fn>;
    operationsByRegistryId: ReturnType<typeof signal<Record<string, AgentOperation>>>;
    clearOperation: ReturnType<typeof vi.fn>;
  };

  beforeEach(async () => {
    state = {
      registry: signal<RegistryCatalog | null>(catalog),
      registryLoading: signal(false),
      registryError: signal<string | null>(null),
      loadRegistry: vi.fn(async () => undefined),
      refreshRegistry: vi.fn(async () => undefined),
      installRegistryAgent: vi.fn(async () => ({ id: 'native-agent' })),
      updateAgent: vi.fn(async () => ({ updated: true, from_version: '1.0.0', to_version: '1.2.0', agent: { id: 'example-acp' } })),
      removeAgent: vi.fn(async () => ({ id: 'example-acp', deleted: true, retained_chats: 0 })),
      operationsByRegistryId: signal<Record<string, AgentOperation>>({}),
      clearOperation: vi.fn(),
    };

    await TestBed.configureTestingModule({
      imports: [RegistryBrowserComponent],
      providers: [
        { provide: AppStateService, useValue: state },
        { provide: MatDialog, useValue: { open: vi.fn(() => ({ afterClosed: () => of(true) })) } },
      ],
    }).compileComponents();

    fixture = TestBed.createComponent(RegistryBrowserComponent);
    fixture.detectChanges();
  });

  function buttonByText(text: string): HTMLButtonElement {
    const button = (Array.from(fixture.nativeElement.querySelectorAll('button')) as HTMLButtonElement[])
      .find((candidate) => candidate.textContent?.trim().includes(text));
    if (!button) throw new Error(`button not found: ${text}`);
    return button;
  }

  it('lists entries with installed, update, and unsupported state', () => {
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Example ACP');
    expect(text).toContain('Installed as example-acp');
    expect(text).toContain('No binary distribution covers this platform.');
  });

  it('renders a production response without an empty rejected field', () => {
    const { rejected, ...response } = catalog;
    state.registry.set(response);
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelectorAll('.entry')).toHaveLength(3);
  });

  function search(query: string): void {
    const input = fixture.nativeElement.querySelector('input') as HTMLInputElement;
    input.value = query;
    input.dispatchEvent(new Event('input'));
    fixture.detectChanges();
  }

  it('filters by name, id, and description as the user types without a request', () => {
    for (const query of ['NATIVE AGENT', 'native-agent', 'A native agent.']) {
      search(query);
      expect(fixture.nativeElement.querySelectorAll('.entry')).toHaveLength(1);
      expect(fixture.nativeElement.querySelector('.entry-name').textContent).toBe('Native Agent');
    }
    expect(state.loadRegistry).not.toHaveBeenCalled();
    expect(state.refreshRegistry).not.toHaveBeenCalled();
  });

  it('clears the query and restores all entries without a request', () => {
    search('no match');
    expect(fixture.nativeElement.querySelectorAll('.entry')).toHaveLength(0);
    fixture.nativeElement.querySelector('button[aria-label="Clear search"]').click();
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelectorAll('.entry')).toHaveLength(3);
    expect(state.loadRegistry).not.toHaveBeenCalled();
    expect(state.refreshRegistry).not.toHaveBeenCalled();
  });

  it('refreshes the full catalog and reapplies the current local query', async () => {
    search('native');
    state.refreshRegistry.mockImplementationOnce(async () => {
      state.registry.set({ ...catalog, status: 'fresh', agents: [
        ...catalog.agents,
        { ...catalog.agents[1], id: 'new-native', name: 'New Native' },
      ] });
    });
    await fixture.componentInstance.refresh();
    fixture.detectChanges();
    expect(state.refreshRegistry).toHaveBeenCalled();
    expect(fixture.componentInstance.query()).toBe('native');
    expect(fixture.nativeElement.querySelectorAll('.entry')).toHaveLength(2);
    search('');
    expect(fixture.nativeElement.querySelectorAll('.entry')).toHaveLength(4);
  });

  it('shows the fetch timestamp and rejection reasons', () => {
    state.registry.set({ ...catalog, fetched_at: '2026-09-15T12:00:00Z',
      rejected: [{ id: 'bad-entry', reason: 'no usable distribution' }] });
    fixture.detectChanges();
    expect(fixture.nativeElement.querySelector('time').getAttribute('datetime')).toBe('2026-09-15T12:00:00Z');
    expect(fixture.nativeElement.querySelector('details').textContent).toContain('bad-entry: no usable distribution');
  });

  it('installs an uninstalled entry through the store and reports success', async () => {
    await fixture.componentInstance.install(catalog.agents[1]);
    expect(state.installRegistryAgent).toHaveBeenCalledWith(expect.objectContaining({
      registry_id: 'native-agent',
      distribution: 'binary',
    }));
    expect(fixture.componentInstance.notice()).toContain('Installed Native Agent');
  });

  it('updates an installed entry and reports when it is already current', async () => {
    await fixture.componentInstance.update(catalog.agents[0]);
    expect(state.updateAgent).toHaveBeenCalledWith('example-acp');

    state.updateAgent.mockResolvedValueOnce({ updated: false, from_version: '1.2.0', to_version: '1.2.0', agent: { id: 'example-acp' } });
    await fixture.componentInstance.update(catalog.agents[0]);
    expect(fixture.componentInstance.notice()).toContain('already at the newest version');
  });

  it('confirms before uninstalling an installed entry', async () => {
    await fixture.componentInstance.uninstall(catalog.agents[0]);
    expect(state.removeAgent).toHaveBeenCalledWith('example-acp');
  });

  it('surfaces install errors', async () => {
    state.installRegistryAgent.mockRejectedValueOnce(new Error('integrity check failed'));
    await fixture.componentInstance.install(catalog.agents[1]);
    expect(fixture.componentInstance.actionError()).toContain('integrity check failed');
  });

  it('never offers an install control for an unsupported entry', () => {
    const installButtons = (Array.from(fixture.nativeElement.querySelectorAll('button')) as HTMLButtonElement[])
      .filter((button) => button.textContent?.trim() === 'Install');
    expect(installButtons).toHaveLength(1);
  });

  it('shows an enabled Retry button when no catalog is available', () => {
    state.registry.set({
      status: 'unavailable',
      source_url: 'https://registry.example.invalid',
      host: 'linux-x86_64',
      rejected: [],
      agents: [],
      error: 'DNS lookup failed',
    });
    state.registryError.set('DNS lookup failed');
    fixture.detectChanges();

    const retry = buttonByText('Retry');
    expect(retry.disabled).toBe(false);
    expect(fixture.nativeElement.textContent).toContain('DNS lookup failed');
    retry.click();
    expect(state.refreshRegistry).toHaveBeenCalled();
  });

  it('keeps the cached catalog and shows one refresh failure', () => {
    state.registry.set({ ...catalog, error: 'Network is unreachable' });
    state.registryError.set('Network is unreachable');
    fixture.detectChanges();

    expect(fixture.nativeElement.textContent).toContain('Example ACP');
    expect(fixture.nativeElement.querySelectorAll('[role="alert"]')).toHaveLength(1);
    expect(fixture.nativeElement.textContent).toContain('Network is unreachable');
  });
  it('displays determinate progress bar, stage text, and byte count for an active download', () => {
    state.operationsByRegistryId.set({
      'native-agent': {
        id: 'op-1',
        kind: 'install',
        agent_id: 'native-agent',
        registry_id: 'native-agent',
        state: 'running',
        stage: 'downloading',
        bytes_downloaded: 2621440,
        total_bytes: 5242880,
        error: null,
        created_at: '',
        updated_at: '',
      },
    });
    fixture.detectChanges();

    const progress = fixture.nativeElement.querySelector('.entry-progress');
    expect(progress).not.toBeNull();
    expect(progress.textContent).toContain('Downloading (2.5 / 5.0 MB)...');
    expect(progress.textContent).toContain('50%');

    const progressBar = progress.querySelector('mat-progress-bar');
    expect(progressBar?.getAttribute('mode')).toBe('determinate');
  });

  it('displays indeterminate progress bar for stages without known total bytes', () => {
    state.operationsByRegistryId.set({
      'native-agent': {
        id: 'op-1',
        kind: 'install',
        agent_id: 'native-agent',
        registry_id: 'native-agent',
        state: 'running',
        stage: 'extracting',
        bytes_downloaded: 5242880,
        total_bytes: null,
        error: null,
        created_at: '',
        updated_at: '',
      },
    });
    fixture.detectChanges();

    const progress = fixture.nativeElement.querySelector('.entry-progress');
    expect(progress).not.toBeNull();
    expect(progress.textContent).toContain('Extracting...');

    const progressBar = progress.querySelector('mat-progress-bar');
    expect(progressBar?.getAttribute('mode')).toBe('indeterminate');
  });

  it('disables actions only on the affected card while unrelated entries remain usable', () => {
    state.operationsByRegistryId.set({
      'native-agent': {
        id: 'op-1',
        kind: 'install',
        agent_id: 'native-agent',
        registry_id: 'native-agent',
        state: 'running',
        stage: 'downloading',
        bytes_downloaded: 100,
        total_bytes: 500,
        error: null,
        created_at: '',
        updated_at: '',
      },
    });
    fixture.detectChanges();

    const buttons = Array.from(fixture.nativeElement.querySelectorAll('button')) as HTMLButtonElement[];
    const installBtn = buttons.find((b) => b.textContent?.trim() === 'Install');
    const updateBtn = buttons.find((b) => b.textContent?.trim() === 'Update');
    const uninstallBtn = buttons.find((b) => b.textContent?.trim() === 'Uninstall');

    expect(installBtn?.disabled).toBe(true);
    expect(updateBtn?.disabled).toBe(false);
    expect(uninstallBtn?.disabled).toBe(false);
  });

  it('displays entry error on failure and keeps the card usable', () => {
    fixture.componentInstance.setCardError('native-agent', 'Integrity verification failed');
    fixture.detectChanges();

    const errorEl = fixture.nativeElement.querySelector('.entry-error');
    expect(errorEl?.textContent).toContain('Integrity verification failed');

    const buttons = Array.from(fixture.nativeElement.querySelectorAll('button')) as HTMLButtonElement[];
    const installBtn = buttons.find((b) => b.textContent?.trim() === 'Install');
    expect(installBtn?.disabled).toBe(false);
  });
});
