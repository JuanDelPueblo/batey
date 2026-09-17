import { BreakpointObserver } from '@angular/cdk/layout';
import { signal } from '@angular/core';
import { TestBed } from '@angular/core/testing';
import { provideRouter, Router } from '@angular/router';
import { of } from 'rxjs';
import { describe, expect, it, beforeEach, vi } from 'vitest';
import { AppStateService } from './state/app-state.service';
import { routes } from './app.routes';

describe('Batey routes', () => {
  let router: Router;

  beforeEach(() => {
    const state = {
      agents: signal([]),
      agentError: signal(null),
      agentsLoading: signal(false),
      authByAgent: signal({}),
      authLoading: signal(new Set<string>()),
      authErrors: signal({}),
      findChat: vi.fn(() => null),
      loadAgents: vi.fn(async () => undefined),
      loadRegistry: vi.fn(async () => undefined),
      loadAgentAuth: vi.fn(async () => undefined),
      operationsByRegistryId: signal({}),
      reducersByChat: signal({}),
      configOptionsByChat: signal({}),
      connectingChats: signal(new Set<string>()),
      connectErrors: signal<Record<string, string>>({}),
      configLoadedByChat: signal<Record<string, boolean>>({}),
      setMobileDrawerOpen: vi.fn(),
      stopChatProcess: vi.fn(async () => undefined),
      cancelActiveTurn: vi.fn(async () => undefined),
      sendPrompt: vi.fn(async () => undefined),
      respondPermission: vi.fn(async () => undefined),
      setChatPolicy: vi.fn(async () => undefined),
      setChatConfig: vi.fn(async () => undefined),
      renameChat: vi.fn(async () => undefined),
      archiveChat: vi.fn(async () => undefined),
      deleteChat: vi.fn(async () => undefined),
    } as unknown as AppStateService;

    TestBed.configureTestingModule({
      providers: [
        provideRouter(routes),
        { provide: AppStateService, useValue: state },
        { provide: BreakpointObserver, useValue: { observe: () => of({ matches: false }) } },
      ],
    });
    router = TestBed.inject(Router);
  });

  it('lazy-loads every page component', async () => {
    expect(routes.slice(0, 5).every((route) => route.loadComponent && !route.component)).toBe(true);
  });

  it('navigates the supported project, chat, agents, and registry URLs and redirects unknown paths home', async () => {
    await router.navigateByUrl('/projects/project-1');
    expect(router.url).toBe('/projects/project-1');

    await router.navigateByUrl('/projects/project-1/chats/chat-1');
    expect(router.url).toBe('/projects/project-1/chats/chat-1');

    await router.navigateByUrl('/agents');
    expect(router.url).toBe('/agents');

    await router.navigateByUrl('/agents/registry');
    expect(router.url).toBe('/agents/registry');

    await router.navigateByUrl('/not-a-route');
    expect(router.url).toBe('/');
  });
});
