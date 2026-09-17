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
    operationsByAgent: ReturnType<typeof signal<Record<string, AgentOperation>>>;
    operationForAgent: ReturnType<typeof vi.fn>;
    operationForRegistry: ReturnType<typeof vi.fn>;
    isAgentBusy: ReturnType<typeof vi.fn>;
    loadRegistry: ReturnType<typeof vi.fn>;
    refreshRegistry: ReturnType<typeof vi.fn>;
    installRegistryAgent: ReturnType<typeof vi.fn>;
    updateAgent: ReturnType<typeof vi.fn>;
    removeAgent: ReturnType<typeof vi.fn>;
  };

  beforeEach(async () => {
    const opsSignal = signal<Record<string, AgentOperation>>({});
    state = {
      registry: signal<RegistryCatalog | null>(catalog),
      registryLoading: signal(false),
      registryError: signal<string | null>(null),
      operationsByAgent: opsSignal,
      operationForAgent: vi.fn((key: string) => opsSignal()[key] ?? null),
      operationForRegistry: vi.fn((key: string) => opsSignal()[key] ?? null),
      isAgentBusy: vi.fn((key: string) => {
        const op = opsSignal()[key];
        return op !== undefined && op.state === 'running';
      }),
      loadRegistry: vi.fn(async () => undefined),
      refreshRegistry: vi.fn(async () => undefined),
      installRegistryAgent: vi.fn(async () => ({
        id: 'op-1',
        agent_id: 'native-agent',
        registry_id: 'native-agent',
        kind: 'install',
        state: 'succeeded',
        stage: 'completed',
        downloaded_bytes: 100,
        total_bytes: 100,
        error: null,
        started_at: '',
        completed_at: '',
      } as AgentOperation)),
      updateAgent: vi.fn(async () => ({
        id: 'op-2',
        agent_id: 'example-acp',
        registry_id: 'example-acp',
        kind: 'update',
        state: 'succeeded',
        stage: 'completed',
        downloaded_bytes: 100,
        total_bytes: 100,
        error: null,
        started_at: '',
        completed_at: '',
        updated: true,
        to_version: '1.2.0',
      } as AgentOperation)),
      removeAgent: vi.fn(async () => ({ id: 'example-acp', deleted: true, retained_chats: 0 })),
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

    state.updateAgent.mockResolvedValueOnce({
      id: 'op-2',
      agent_id: 'example-acp',
      registry_id: 'example-acp',
      kind: 'update',
      state: 'succeeded',
      stage: 'completed',
      downloaded_bytes: 100,
      total_bytes: 100,
      error: null,
      started_at: '',
      completed_at: '',
      updated: false,
      to_version: null,
    } as AgentOperation);
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
    expect(fixture.componentInstance.actionError()).toBe('');
    expect(fixture.componentInstance.errorFor(catalog.agents[1])).toContain('integrity check failed');
  });

  it('keeps an entry busy until its initial install request settles', async () => {
    let resolveInstall!: (operation: AgentOperation) => void;
    state.installRegistryAgent.mockImplementationOnce(() => new Promise((resolve) => {
      resolveInstall = resolve;
    }));

    const installing = fixture.componentInstance.install(catalog.agents[1]);
    expect(fixture.componentInstance.isEntryBusy(catalog.agents[1])).toBe(true);
    resolveInstall({ state: 'succeeded' } as AgentOperation);
    await installing;
    expect(fixture.componentInstance.isEntryBusy(catalog.agents[1])).toBe(false);
  });

  it('keeps an entry busy until its initial update request settles', async () => {
    let resolveUpdate!: (operation: AgentOperation) => void;
    state.updateAgent.mockImplementationOnce(() => new Promise((resolve) => {
      resolveUpdate = resolve;
    }));

    const updating = fixture.componentInstance.update(catalog.agents[0]);
    expect(fixture.componentInstance.isEntryBusy(catalog.agents[0])).toBe(true);
    resolveUpdate({ state: 'succeeded', updated: false } as AgentOperation);
    await updating;
    expect(fixture.componentInstance.isEntryBusy(catalog.agents[0])).toBe(false);
  });

  it('displays determinate progress and disables buttons only for the active agent', () => {
    state.operationsByAgent.set({
      'native-agent': {
        id: 'op-1',
        agent_id: 'native-agent',
        registry_id: 'native-agent',
        kind: 'install',
        state: 'running',
        stage: 'downloading',
        downloaded_bytes: 5242880,
        total_bytes: 10485760,
        error: null,
        started_at: '',
        completed_at: null,
      },
    });
    fixture.detectChanges();

    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Downloading... (5.0 MB / 10.0 MB)');
    expect(text).toContain('50%');

    // Native agent install button is disabled
    const installBtn = (Array.from(fixture.nativeElement.querySelectorAll('button')) as HTMLButtonElement[])
      .find((b) => b.textContent?.trim() === 'Install');
    expect(installBtn?.disabled).toBe(true);

    // Example ACP update button is NOT disabled
    const updateBtn = (Array.from(fixture.nativeElement.querySelectorAll('button')) as HTMLButtonElement[])
      .find((b) => b.textContent?.trim() === 'Update');
    expect(updateBtn?.disabled).toBe(false);
  });

  it('displays indeterminate progress for extracting stage', () => {
    state.operationsByAgent.set({
      'native-agent': {
        id: 'op-1',
        agent_id: 'native-agent',
        registry_id: 'native-agent',
        kind: 'install',
        state: 'running',
        stage: 'extracting',
        downloaded_bytes: 0,
        total_bytes: null,
        error: null,
        started_at: '',
        completed_at: null,
      },
    });
    fixture.detectChanges();

    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Extracting archive...');
    const progressBar = fixture.nativeElement.querySelector('mat-progress-bar');
    expect(progressBar?.getAttribute('mode')).toBe('indeterminate');
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
});
