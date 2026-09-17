import { ComponentFixture, TestBed } from '@angular/core/testing';
import { signal } from '@angular/core';
import { provideRouter } from '@angular/router';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { AppStateService } from '../../state/app-state.service';
import { AgentRegistryPageComponent } from './agent-registry-page';

describe('AgentRegistryPageComponent', () => {
  let fixture: ComponentFixture<AgentRegistryPageComponent>;
  let loadRegistry: ReturnType<typeof vi.fn>;
  let loadOperations: ReturnType<typeof vi.fn>;

  beforeEach(async () => {
    loadRegistry = vi.fn(async () => undefined);
    loadOperations = vi.fn(async () => undefined);
    const state = {
      registry: signal(null),
      registryLoading: signal(false),
      registryError: signal<string | null>(null),
      loadRegistry,
      loadOperations,
    };

    await TestBed.configureTestingModule({
      imports: [AgentRegistryPageComponent],
      providers: [provideRouter([]), { provide: AppStateService, useValue: state }],
    }).compileComponents();

    fixture = TestBed.createComponent(AgentRegistryPageComponent);
    fixture.detectChanges();
  });

  it('carries a clear title and subtitle', () => {
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Agent Registry');
    expect(text).toContain('Discover and install ACP agents');
  });

  it('links back to the installed agents page', () => {
    const back = fixture.nativeElement.querySelector('a[href="/agents"]');
    expect(back).not.toBeNull();
    expect(back.textContent).not.toBe('');
    expect(fixture.nativeElement.querySelector('a[aria-label="Back to installed agents"]')).not.toBeNull();
  });

  it('renders the shared registry browser', () => {
    expect(fixture.nativeElement.querySelector('hub-registry-browser')).not.toBeNull();
  });

  it('loads the registry catalog when the page opens', () => {
    expect(loadRegistry).toHaveBeenCalled();
  });

  it('recovers active operations when the page opens', () => {
    expect(loadOperations).toHaveBeenCalled();
  });
});
