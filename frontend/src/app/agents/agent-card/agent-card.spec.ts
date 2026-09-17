import { ComponentFixture, TestBed } from '@angular/core/testing';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { AgentAuthState, AgentSummary } from '../../core/api/types';
import { AgentCardComponent } from './agent-card';

function summary(source: AgentSummary['source'], overrides: Partial<AgentSummary> = {}): AgentSummary {
  return {
    id: `${source}-agent`,
    display_name: `${source} agent`,
    source,
    availability: 'available',
    metadata: {},
    ...overrides,
  } as AgentSummary;
}

describe('AgentCardComponent', () => {
  let fixture: ComponentFixture<AgentCardComponent>;

  beforeEach(async () => {
    await TestBed.configureTestingModule({ imports: [AgentCardComponent] }).compileComponents();
    fixture = TestBed.createComponent(AgentCardComponent);
  });

  function render(agent: AgentSummary, auth: AgentAuthState | null = null): void {
    fixture.componentRef.setInput('agent', agent);
    fixture.componentRef.setInput('auth', auth);
    fixture.detectChanges();
  }

  it('presents built-in sources as read-only without mutation controls', () => {
    render(summary('builtin', { display: { description: 'A builtin.' } }));
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Built-in');
    expect(text).toContain('Read-only');
    expect(text).toContain('A builtin.');
    expect(text).toContain('Defined outside Batey');
    const actionLabels = (Array.from(fixture.nativeElement.querySelectorAll('button')) as HTMLButtonElement[])
      .map((button) => button.textContent?.trim() ?? '')
      .join(' ');
    expect(actionLabels).not.toContain('Edit');
    expect(actionLabels).not.toContain('Remove');
    expect(actionLabels).not.toContain('Uninstall');
    expect(actionLabels).not.toContain('Environment');
  });

  it('offers edit and remove for an editable custom agent', () => {
    const edit = vi.fn();
    const remove = vi.fn();
    fixture.componentInstance.edit.subscribe(edit);
    fixture.componentInstance.remove.subscribe(remove);
    render(summary('batey_managed'));
    const buttons = Array.from(fixture.nativeElement.querySelectorAll('button')) as HTMLButtonElement[];
    const actionLabels = buttons.map((button) => button.textContent?.trim() ?? '').join(' ');
    expect(actionLabels).toContain('Edit');
    expect(actionLabels).toContain('Remove');
    buttons.find((button) => button.textContent?.includes('Edit'))?.click();
    buttons.find((button) => button.textContent?.includes('Remove'))?.click();
    expect(edit).toHaveBeenCalled();
    expect(remove).toHaveBeenCalled();
  });

  it('offers update and uninstall for a registry-managed agent', () => {
    render(summary('registry'));
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Registry-managed');
    expect(text).toContain('Update');
    expect(text).toContain('Uninstall');
    expect(text).toContain('Environment');
    expect(text).not.toContain('Edit');
  });

  it('offers environment settings for editable and registry-managed agents', () => {
    const environment = vi.fn();
    fixture.componentInstance.environment.subscribe(environment);
    render(summary('batey_managed'));
    let buttons = Array.from(fixture.nativeElement.querySelectorAll('button')) as HTMLButtonElement[];
    expect(buttons.map((button) => button.textContent?.trim() ?? '').join(' ')).toContain('Environment');
    buttons.find((button) => button.textContent?.includes('Environment'))?.click();
    expect(environment).toHaveBeenCalled();
  });

  it('shows an unavailable reason', () => {
    render(summary('builtin', { availability: 'unavailable', unavailable_reason: 'Command missing.' }));
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Unavailable');
    expect(text).toContain('Command missing.');
  });

  it('keeps availability as the single status treatment in the header', () => {
    render(summary('builtin', { display: { description: 'A builtin.' } }));
    const head = fixture.nativeElement.querySelector('.card-head') as HTMLElement;
    expect(head.textContent).toContain('Available');
    expect(head.textContent).not.toContain('Read-only');
    expect(head.textContent).not.toContain('Built-in');
    expect(head.querySelector('.availability')).not.toBeNull();
    const provenance = fixture.nativeElement.querySelector('.provenance') as HTMLElement;
    expect(provenance.textContent).toContain('Source: Built-in');
    expect(provenance.textContent).toContain('Read-only');
    expect(fixture.nativeElement.querySelectorAll('.availability').length).toBe(1);
  });

  it('keeps a targeted sign-in section focusable for authentication deep links', () => {
    fixture.componentRef.setInput('agent', summary('builtin'));
    fixture.componentRef.setInput('targeted', true);
    fixture.detectChanges();
    const auth = fixture.nativeElement.querySelector('.auth') as HTMLElement;
    expect(auth.id).toBe('agent-auth-builtin-agent');
    expect(auth.tabIndex).toBe(-1);
    expect(auth.classList.contains('targeted')).toBe(true);
  });

  it('represents agent, terminal, and unsupported methods honestly', () => {
    const auth: AgentAuthState = {
      agent_id: 'x',
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'unknown',
      freshness: 'cached',
      observed_freshness: 'cached',
      methods: [
        { id: 'oauth', name: 'OAuth', type: 'agent', supported: true },
        { id: 'key', name: 'API key', type: 'terminal', supported: true },
        { id: 'device', name: 'Device flow', type: 'device_code', supported: false },
        { id: 'term-unsupported', name: 'No PTY', type: 'terminal', supported: false },
      ],
    };
    render(summary('builtin'), auth);
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Sign in');
    expect(text).toContain('Open terminal');
    expect(text).toContain('Unsupported (device_code)');
    expect(text).toContain('Terminal (unsupported)');

    const disabledButtons = (Array.from(fixture.nativeElement.querySelectorAll('button')) as HTMLButtonElement[])
      .filter((button) => button.textContent?.includes('Unsupported'));
    expect(disabledButtons.length).toBe(2);
    expect(disabledButtons[0]?.disabled).toBe(true);
    expect(disabledButtons[1]?.disabled).toBe(true);
  });

  it('shows logout only when authenticated and clear action when auth was required', () => {
    const logout = vi.fn();
    const clear = vi.fn();
    fixture.componentInstance.logout.subscribe(logout);
    fixture.componentInstance.clearCredentials.subscribe(clear);
    // Unknown state is an internal absence of evidence: no status claim and
    // no Log out, only the lower-emphasis clear action when the capability
    // exists and authentication was actually required.
    render(summary('builtin'), {
      agent_id: 'x',
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'authentication_required',
      freshness: 'cached',
      observed_freshness: 'cached',
      methods: [],
    });
    let text = fixture.nativeElement.textContent as string;
    expect(text).not.toContain('Active session');
    expect(text).toContain('Authentication required');
    expect(
      Array.from(fixture.nativeElement.querySelectorAll('button')).find((item) =>
        (item as HTMLButtonElement).textContent?.includes('Log out'),
      ),
    ).toBeUndefined();
    const clearButton = Array.from(fixture.nativeElement.querySelectorAll('button')).find((item) =>
      (item as HTMLButtonElement).textContent?.includes('Clear saved sign-in'),
    ) as HTMLButtonElement;
    expect(clearButton).toBeDefined();
    clearButton.click();
    expect(clear).toHaveBeenCalled();
    expect(logout).not.toHaveBeenCalled();

    // Authenticated state: the normal Log out appears.
    render(summary('builtin'), {
      agent_id: 'x',
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'authenticated',
      freshness: 'cached',
      observed_freshness: 'cached',
      methods: [],
    });
    text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Authenticated');
    const logoutButton = Array.from(fixture.nativeElement.querySelectorAll('button')).find((item) =>
      (item as HTMLButtonElement).textContent?.includes('Log out'),
    ) as HTMLButtonElement;
    expect(logoutButton).toBeDefined();
    logoutButton.click();
    expect(logout).toHaveBeenCalled();
  });

  it('never renders an unknown sign-in status label', () => {
    render(summary('builtin'), {
      agent_id: 'x',
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'unknown',
      freshness: 'cached',
      observed_freshness: 'cached',
      methods: [{ id: 'oauth', name: 'OAuth', type: 'agent', supported: true }],
    });
    const text = fixture.nativeElement.textContent as string;
    expect(text).not.toContain('Sign-in status unknown');
    expect(text).not.toContain('unknown');
    // Methods still show normally under the absence-of-evidence state.
    expect(text).toContain('OAuth');
    const status = fixture.nativeElement.querySelector('.auth-status');
    expect(status).toBeNull();
  });

  it('hides sign-in methods and shows only Log out once authenticated', () => {
    render(summary('builtin'), {
      agent_id: 'x',
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'authenticated',
      freshness: 'cached',
      observed_freshness: 'cached',
      methods: [
        { id: 'oauth', name: 'OAuth', type: 'agent', supported: true },
        { id: 'tui', name: 'Terminal', type: 'terminal', supported: true },
      ],
    });
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Authenticated');
    expect(text).not.toContain('OAuth');
    expect(text).not.toContain('Open terminal');
    const labels = (Array.from(fixture.nativeElement.querySelectorAll('button')) as HTMLButtonElement[])
      .map((button) => button.textContent?.trim() ?? '');
    expect(labels.filter((label) => label === 'Log out').length).toBe(1);
    expect(labels.some((label) => label.includes('Sign in'))).toBe(false);
  });

  it('restores sign-in methods after observed auth-required', () => {
    render(summary('builtin'), {
      agent_id: 'x',
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'authentication_required',
      freshness: 'cached',
      observed_freshness: 'cached',
      methods: [{ id: 'oauth', name: 'OAuth', type: 'agent', supported: true }],
    });
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Authentication required');
    expect(text).toContain('OAuth');
    expect(text).toContain('Sign in');
  });

  it('does not render the removed compatibility warning field', () => {
    render(summary('builtin'), {
      agent_id: 'x',
      logout_supported: false,
      terminal_supported: true,
      observed_state: 'unknown',
      freshness: 'cached',
      observed_freshness: 'cached',
      methods: [
        {
          id: 'interactive',
          name: 'Interactive sign-in',
          type: 'agent',
          supported: true,
        },
      ],
    });
    const text = fixture.nativeElement.textContent as string;
    expect(text).not.toContain('compatibility warning');
    expect(text).toContain('Interactive sign-in');
  });

  it('offers Resume terminal and Cancel for a recovered running terminal flow', () => {
    const resume = vi.fn();
    const cancel = vi.fn();
    fixture.componentInstance.resumeTerminal.subscribe(resume);
    fixture.componentInstance.cancelTerminal.subscribe(cancel);
    fixture.componentRef.setInput('agent', summary('builtin'));
    fixture.componentRef.setInput('auth', {
      agent_id: 'x',
      logout_supported: false,
      terminal_supported: true,
      observed_state: 'unknown',
      freshness: 'cached',
      observed_freshness: 'cached',
      methods: [{ id: 'tui', name: 'Terminal', type: 'terminal', supported: true }],
    });
    fixture.componentRef.setInput('terminalFlow', {
      flow_id: 'flow-7',
      agent_id: 'x',
      method_id: 'tui',
      state: 'running',
    });
    fixture.detectChanges();
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Authentication is still running in a terminal');
    const buttons = Array.from(fixture.nativeElement.querySelectorAll('button')) as HTMLButtonElement[];
    buttons.find((button) => button.textContent?.includes('Resume terminal'))?.click();
    expect(resume).toHaveBeenCalledWith('flow-7');
    buttons.find((button) => button.textContent?.trim() === 'Cancel')?.click();
    expect(cancel).toHaveBeenCalled();
  });

  it('shows simple wording when an agent offers no sign-in options', () => {
    render(summary('builtin'), {
      agent_id: 'x',
      logout_supported: false,
      terminal_supported: false,
      observed_state: 'unknown',
      freshness: 'cached',
      observed_freshness: 'cached',
      methods: [],
    });
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('No sign-in options available.');
    expect(text).not.toContain('reports no authentication methods');
  });

  it('offers a check action when sign-in state is not loaded', () => {
    const retryAuth = vi.fn();
    fixture.componentInstance.retryAuth.subscribe(retryAuth);
    render(summary('builtin'), null);
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Sign-in status is not loaded.');
    const button = Array.from(fixture.nativeElement.querySelectorAll('button'))
      .find((item) => (item as HTMLButtonElement).textContent?.includes('Check')) as HTMLButtonElement;
    expect(button).toBeDefined();
    button.click();
    expect(retryAuth).toHaveBeenCalled();
  });

  it('truthfully reports a never-checked agent instead of claiming no methods exist', () => {
    const retryAuth = vi.fn();
    fixture.componentInstance.retryAuth.subscribe(retryAuth);
    render(summary('builtin'), {
      agent_id: 'x',
      logout_supported: false,
      terminal_supported: true,
      observed_state: 'unknown',
      freshness: 'unknown',
      observed_freshness: 'unknown',
      methods: [],
    });
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Sign-in status has not been checked yet.');
    expect(text).not.toContain('No sign-in options available.');
    const button = Array.from(fixture.nativeElement.querySelectorAll('button'))
      .find((item) => (item as HTMLButtonElement).textContent?.includes('Check')) as HTMLButtonElement;
    expect(button).toBeDefined();
    button.click();
    expect(retryAuth).toHaveBeenCalled();
  });

  it('shows stale authenticated evidence as historical, never as a fresh check', () => {
    render(summary('builtin'), {
      agent_id: 'x',
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'authenticated',
      freshness: 'stale',
      observed_freshness: 'stale',
      methods: [],
    });
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Previously signed in');
    expect(text).not.toContain('Authenticated');
  });

  it('shows stale authentication-required evidence as historical too, not as a current claim', () => {
    render(summary('builtin'), {
      agent_id: 'x',
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'authentication_required',
      freshness: 'stale',
      observed_freshness: 'stale',
      methods: [],
    });
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Previously required sign-in');
    expect(text).not.toContain('Authentication required');
  });

  it('shows checking sign-in while a protocol flow starts, unless the flow is available', () => {
    fixture.componentRef.setInput('agent', summary('builtin'));
    fixture.componentRef.setInput('auth', {
      agent_id: 'x',
      logout_supported: false,
      terminal_supported: false,
      observed_state: 'unknown',
      freshness: 'cached',
      observed_freshness: 'cached',
      methods: [{ id: 'oauth', name: 'OAuth', type: 'agent', supported: true }],
    });
    fixture.componentRef.setInput('protocolLoading', true);
    fixture.detectChanges();
    expect(fixture.nativeElement.textContent).toContain('Checking sign-in…');

    fixture.componentRef.setInput('protocolFlow', {
      flow_id: 'flow-1',
      agent_id: 'x',
      method_id: 'oauth',
      state: 'running',
      reason: null,
    });
    fixture.detectChanges();
    expect(fixture.nativeElement.textContent).toContain('Signing in…');
    expect(fixture.nativeElement.textContent).not.toContain('Checking sign-in…');
  });

  it('places each method label on the left and its action on the right', () => {
    render(summary('builtin'), {
      agent_id: 'x',
      logout_supported: false,
      terminal_supported: true,
      observed_state: 'unknown',
      freshness: 'cached',
      observed_freshness: 'cached',
      methods: [{ id: 'oauth', name: 'OAuth', type: 'agent', supported: true }],
    });
    const rows = fixture.nativeElement.querySelectorAll('.method') as NodeListOf<HTMLElement>;
    expect(rows.length).toBe(1);
    const text = rows[0].querySelector('.method-text');
    const action = rows[0].querySelector('.method-action');
    expect(text).not.toBeNull();
    expect(action).not.toBeNull();
    expect(rows[0].querySelectorAll('button').length).toBe(1);
  });

  it('shows version and usage provider metadata', () => {
    render(summary('builtin', { usage_provider: 'openai', display: { version: '1.2.3' } }));
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Version');
    expect(text).toContain('1.2.3');
    expect(text).toContain('Usage provider');
    expect(text).toContain('openai');
  });

  it('hides logout when logout is not supported', () => {
    render(summary('builtin'), {
      agent_id: 'x',
      logout_supported: false,
      terminal_supported: true,
      observed_state: 'unknown',
      freshness: 'cached',
      observed_freshness: 'cached',
      methods: [],
    });
    const button = Array.from(fixture.nativeElement.querySelectorAll('button'))
      .find((item) => (item as HTMLButtonElement).textContent?.includes('Log out'));
    expect(button).toBeUndefined();
  });

  it('renders repository, website, and license links', () => {
    render(summary('builtin', {
      display: {
        repository: 'https://example.invalid/repo',
        website: 'https://example.invalid',
        license: 'MIT',
        license_url: 'https://example.invalid/license',
      },
    }));
    const links = (fixture.nativeElement.querySelectorAll('a') as NodeListOf<HTMLAnchorElement>);
    const hrefs = Array.from(links).map((link) => link.getAttribute('href'));
    expect(hrefs).toContain('https://example.invalid/repo');
    expect(hrefs).toContain('https://example.invalid');
    expect(hrefs).toContain('https://example.invalid/license');
  });

  it('never presents sign-in and log out together from capability alone', () => {
    render(summary('builtin'), {
      agent_id: 'x',
      logout_supported: true,
      terminal_supported: true,
      observed_state: 'unknown',
      freshness: 'cached',
      observed_freshness: 'cached',
      methods: [{ id: 'oauth', name: 'OAuth', type: 'agent', supported: true }],
    });
    const buttons = Array.from(fixture.nativeElement.querySelectorAll('button')).map(
      (button) => (button as HTMLButtonElement).textContent?.trim() ?? '',
    );
    expect(buttons.some((label) => label.includes('Sign in'))).toBe(true);
    expect(buttons.some((label) => label === 'Log out')).toBe(false);
  });

  it('shows a cancellable waiting flow with the full URL and host', () => {
    const cancel = vi.fn();
    const respond = vi.fn();
    fixture.componentInstance.cancelProtocol.subscribe(cancel);
    fixture.componentInstance.respondElicitation.subscribe(respond);
    fixture.componentRef.setInput('agent', summary('builtin'));
    fixture.componentRef.setInput('auth', {
      agent_id: 'x',
      logout_supported: false,
      terminal_supported: true,
      observed_state: 'unknown',
      freshness: 'cached',
      observed_freshness: 'cached',
      methods: [{ id: 'oauth', name: 'OAuth', type: 'agent', supported: true }],
    });
    fixture.componentRef.setInput('protocolFlow', {
      flow_id: 'f',
      agent_id: 'x',
      method_id: 'oauth',
      state: 'waiting_for_user',
      reason: null,
    });
    fixture.componentRef.setInput('protocolElicitations', [
      {
        id: 'e1',
        mode: 'url',
        message: 'Open the device page.',
        url: 'https://example.invalid/device?code=ABCD-1234',
        elicitation_id: 'device-1',
        tool_call_id: null,
      },
    ]);
    fixture.detectChanges();
    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain('Waiting for your action');
    expect(text).toContain('https://example.invalid/device?code=ABCD-1234');
    expect(text).toContain('example.invalid');
    expect(text).toContain('Batey never opens or fetches it automatically');
    const elicitations = fixture.nativeElement.querySelector('.elicitations') as HTMLElement;
    expect(elicitations.getAttribute('role')).toBe('group');
    expect(elicitations.getAttribute('aria-label')).toBe('Sign-in action required');
    const buttons = Array.from(fixture.nativeElement.querySelectorAll('button')) as HTMLButtonElement[];
    buttons.find((button) => button.textContent?.includes('Cancel'))?.click();
    expect(cancel).toHaveBeenCalled();
  });

  it('wipes callback drafts when the browser interaction or flow changes', async () => {
    render(summary('builtin'));
    fixture.componentRef.setInput('protocolFlow', {
      flow_id: 'flow-one',
      agent_id: 'builtin-agent',
      method_id: 'oauth',
      state: 'waiting_for_user',
      reason: null,
    });
    fixture.componentRef.setInput('protocolInteraction', {
      type: 'browser',
      url: 'https://accounts.example.test/authorize?state=one',
      manual_callback: true,
    });
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.componentInstance.callbackDraft.set('http://localhost:43123/callback?code=secret&state=one');

    // A replacement flow must clear even when the provider reuses the same
    // authorization URL, and removal must clear the draft too.
    fixture.componentRef.setInput('protocolFlow', {
      flow_id: 'flow-two',
      agent_id: 'builtin-agent',
      method_id: 'oauth',
      state: 'waiting_for_user',
      reason: null,
    });
    fixture.detectChanges();
    await fixture.whenStable();
    expect(fixture.componentInstance.callbackDraft()).toBe('');

    fixture.componentInstance.callbackDraft.set('http://localhost:43123/callback?code=secret&state=one');
    fixture.componentRef.setInput('protocolInteraction', null);
    fixture.detectChanges();
    await fixture.whenStable();
    expect(fixture.componentInstance.callbackDraft()).toBe('');
  });

  it("preserves cached methods and shows honest refresh-failed indicator when refresh fails", async () => {
    render(summary("builtin"), {
      agent_id: "legacy-file",
      logout_supported: false,
      terminal_supported: true,
      observed_state: "unknown",
      freshness: "stale",
      observed_freshness: "cached",
      methods: [{ id: "file-token", name: "File Token", type: "agent", supported: true }],
    });
    fixture.componentRef.setInput("authError", "Agent 'legacy-file' did not start in time");
    fixture.detectChanges();
    await fixture.whenStable();

    const text = fixture.nativeElement.textContent as string;
    expect(text).toContain("Agent 'legacy-file' did not start in time");
    expect(text).toContain("File Token");
    const signInButton = Array.from(fixture.nativeElement.querySelectorAll("button"))
      .find((button) => (button as HTMLButtonElement).textContent?.includes("Sign in")) as HTMLButtonElement;
    expect(signInButton).toBeDefined();

    const retryButton = Array.from(fixture.nativeElement.querySelectorAll("button"))
      .find((button) => (button as HTMLButtonElement).textContent?.includes("Retry")) as HTMLButtonElement;
    expect(retryButton).toBeDefined();
  });

  it("presents Open sign-in page cleanly and localhost callback as fallback only when manual_callback is true", async () => {
    render(summary("builtin"), {
      agent_id: "claude",
      methods: [{ id: "claude-oauth", name: "Claude OAuth", type: "agent", supported: true }],
      logout_supported: false,
      terminal_supported: false,
      observed_state: "unknown",
      freshness: "cached",
      observed_freshness: "cached",
    });
    fixture.componentRef.setInput("protocolFlow", {
      flow_id: "flow-oauth",
      agent_id: "claude",
      method_id: "claude-oauth",
      state: "waiting_for_user",
      reason: null,
    });
    fixture.componentRef.setInput("protocolInteraction", {
      type: "browser",
      url: "https://accounts.anthropic.com/oauth/authorize?client_id=fake",
      manual_callback: true,
    });
    fixture.detectChanges();
    await fixture.whenStable();

    const openLink = fixture.nativeElement.querySelector("a[href^='https://accounts.anthropic.com']") as HTMLAnchorElement;
    expect(openLink).not.toBeNull();
    expect(openLink.textContent).toContain("Open sign-in page");

    expect(fixture.nativeElement.textContent).toContain("Final callback address");
    const sendButton = Array.from(fixture.nativeElement.querySelectorAll("button"))
      .find((button) => (button as HTMLButtonElement).textContent?.includes("Send callback")) as HTMLButtonElement;
    expect(sendButton).toBeDefined();

    fixture.componentRef.setInput("protocolInteraction", {
      type: "browser",
      url: "https://accounts.anthropic.com/oauth/authorize?client_id=fake",
      manual_callback: false,
    });
    fixture.detectChanges();
    await fixture.whenStable();

    expect(fixture.nativeElement.textContent).not.toContain("Final callback address");
  });

  it("provides both Retry and Dismiss actions on failed protocol flow", async () => {
    const authenticate = vi.fn();
    const dismiss = vi.fn();
    fixture.componentInstance.authenticate.subscribe(authenticate);
    fixture.componentInstance.dismissProtocol.subscribe(dismiss);

    render(summary("builtin"), {
      agent_id: "codex",
      methods: [{ id: "openai-oauth", name: "OpenAI OAuth", type: "agent", supported: true }],
      logout_supported: false,
      terminal_supported: false,
      observed_state: "unknown",
      freshness: "cached",
      observed_freshness: "cached",
    });
    fixture.componentRef.setInput("protocolFlow", {
      flow_id: "flow-fail",
      agent_id: "codex",
      method_id: "openai-oauth",
      state: "failed",
      reason: "Sign-in was declined",
    });
    fixture.detectChanges();
    await fixture.whenStable();

    expect(fixture.nativeElement.textContent).toContain("Sign-in was declined");
    const buttons = Array.from(fixture.nativeElement.querySelectorAll("button")) as HTMLButtonElement[];
    const retryBtn = buttons.find((b) => b.textContent?.trim() === "Retry");
    const dismissBtn = buttons.find((b) => b.textContent?.trim() === "Dismiss");
    expect(retryBtn).toBeDefined();
    expect(dismissBtn).toBeDefined();

    retryBtn?.click();
    expect(authenticate).toHaveBeenCalledWith("openai-oauth");

    dismissBtn?.click();
    expect(dismiss).toHaveBeenCalled();
  });
});
