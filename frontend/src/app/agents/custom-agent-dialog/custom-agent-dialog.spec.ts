import { ComponentFixture, TestBed } from '@angular/core/testing';
import { MAT_DIALOG_DATA, MatDialogRef } from '@angular/material/dialog';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { AgentManagementDetail } from '../../core/api/types';
import { AppStateService } from '../../state/app-state.service';
import { CustomAgentDialogComponent } from './custom-agent-dialog';

const detail: AgentManagementDetail = {
  id: 'my-custom',
  display_name: 'My Custom',
  command: 'my-agent',
  args: ['--acp', '--verbose'],
  env: { MY_AGENT_TOKEN: 'secret' },
  idle_timeout: 600,
  usage_provider: 'provider-x',
  metadata: { package: 'my-agent' },
  default_permission_policy: 'read-only',
  description: 'A custom agent.',
};

describe('CustomAgentDialogComponent', () => {
  let fixture: ComponentFixture<CustomAgentDialogComponent>;
  let dialogRef: { close: ReturnType<typeof vi.fn> };
  let state: {
    validateCustomAgent: ReturnType<typeof vi.fn>;
    createCustomAgent: ReturnType<typeof vi.fn>;
    editCustomAgent: ReturnType<typeof vi.fn>;
  };

  async function setup(dataDetail: AgentManagementDetail | null): Promise<void> {
    dialogRef = { close: vi.fn() };
    state = {
      validateCustomAgent: vi.fn(async () => ({ valid: true, issues: [] })),
      createCustomAgent: vi.fn(async () => ({ id: 'created' })),
      editCustomAgent: vi.fn(async () => ({ id: 'my-custom' })),
    };
    await TestBed.configureTestingModule({
      imports: [CustomAgentDialogComponent],
      providers: [
        { provide: MAT_DIALOG_DATA, useValue: { detail: dataDetail } },
        { provide: MatDialogRef, useValue: dialogRef },
        { provide: AppStateService, useValue: state },
      ],
    }).compileComponents();
    fixture = TestBed.createComponent(CustomAgentDialogComponent);
    fixture.detectChanges();
  }

  it('prefills every editable field from management detail', async () => {
    await setup(detail);
    const component = fixture.componentInstance;
    expect(component.id()).toBe('my-custom');
    expect(component.command()).toBe('my-agent');
    expect(component.argsText()).toBe('--acp\n--verbose');
    expect(component.envText()).toBe('MY_AGENT_TOKEN=secret');
    expect(component.idleTimeout()).toBe('600');
    expect(component.description()).toBe('A custom agent.');
  });

  it('validates then creates a new custom agent', async () => {
    await setup(null);
    const component = fixture.componentInstance;
    component.id.set('new-agent');
    component.command.set('new-agent-acp');
    component.argsText.set('--acp\n\n--flag');
    component.envText.set('A=1\nBADLINE\nB=2');
    await component.save();

    expect(state.validateCustomAgent).toHaveBeenCalled();
    expect(state.createCustomAgent).toHaveBeenCalledWith(expect.objectContaining({
      id: 'new-agent',
      command: 'new-agent-acp',
      args: ['--acp', '--flag'],
      env: { A: '1', B: '2' },
    }));
    expect(dialogRef.close).toHaveBeenCalledWith(true);
  });

  it('blocks the save when validation fails', async () => {
    await setup(null);
    state.validateCustomAgent.mockResolvedValueOnce({
      valid: false,
      issues: [{ field: 'command', message: 'An agent needs a command.' }],
    });
    const component = fixture.componentInstance;
    component.id.set('new-agent');
    component.command.set('');
    await component.save();
    expect(state.createCustomAgent).not.toHaveBeenCalled();
    expect(component.issueFor('command')).toBe('An agent needs a command.');
    expect(dialogRef.close).not.toHaveBeenCalled();
  });

  it('edits an existing custom agent without changing its id', async () => {
    await setup(detail);
    const component = fixture.componentInstance;
    component.displayName.set('Renamed');
    await component.save();
    expect(state.editCustomAgent).toHaveBeenCalledWith('my-custom', expect.objectContaining({ display_name: 'Renamed' }));
  });

  it('rejects invalid metadata JSON before saving', async () => {
    await setup(null);
    const component = fixture.componentInstance;
    component.id.set('new-agent');
    component.command.set('cmd');
    component.metadataText.set('{not json');
    await component.save();
    expect(state.validateCustomAgent).not.toHaveBeenCalled();
    expect(component.error()).toContain('valid JSON');
  });
});
