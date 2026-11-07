// A plugin test host: it stands where the Pig Code Engine stands, so a backend
// can be tested through the real SDK protocol (hello → start → ready, tool
// calls and selections, commands, service requests and events, lifecycle health
// and stop, stale envelopes) without a signed package or a running app.
//
// ```ts
// import "../examples/moon-plugin/backend";        // calls apply(...)
// import { createTestHost } from "../sdk/pig-plugin/mod";
//
// const host = await createTestHost().start();
// const echoed = await host.tool("MoonEcho", { text: "hi" });
// ```
//
// Hook contributions registered by the plugin are collected like any other
// runtime contribution (`host.hooks()`), which is what the Rust host runs
// through its ordinary hook runner.

import { runPlugin, takePendingApplier, type PluginApplier, type ToolDefinition } from "./index";
import {
  asPluginError,
  isCurrent,
  PLUGIN_PROTOCOL_VERSION,
  PluginError,
  PluginSequencer,
  validatePluginEnvelope,
  type PluginEnvelope,
  type PluginIdentity,
  type PluginMessageKind,
  type PluginProtocolErrorPayload,
  type PluginStream,
} from "./protocol";

export interface TestHostOptions {
  identity?: Partial<PluginIdentity>;
  applier?: PluginApplier;
  /** How long `start()` waits for `lifecycle.ready` (default 5000 ms). */
  readyTimeoutMs?: number;
  /** How long `tool()`/`select()`/`health()` wait for an answer. */
  callTimeoutMs?: number;
}

export interface ServiceRequestRecord {
  requestId: string;
  service: string;
  payload: unknown;
  sessionId?: string;
  workspaceId?: string;
  idempotencyKey?: string;
}

export interface TestHost {
  readonly ready: boolean;
  readonly identity: PluginIdentity;
  /** Protocol violations or runtime reports seen on the host side. */
  readonly errors: readonly string[];
  /** Delivers an envelope without the host's sequence bookkeeping (stale,
   * foreign-epoch and duplicate-sequence tests). */
  sendRaw(envelope: PluginEnvelope): void;
  /** Every envelope the plugin sent, in order. */
  readonly envelopes: readonly PluginEnvelope[];
  readonly serviceRequests: readonly ServiceRequestRecord[];
  start(): Promise<TestHost>;
  stop(): void;
  tool<Result = unknown>(
    name: string,
    args?: unknown,
    options?: { sessionId?: string },
  ): Promise<Result>;
  select(options?: {
    sessionId?: string;
    agentId?: string;
    model?: Record<string, unknown>;
  }): Promise<string[]>;
  command(id: string): Promise<void>;
  health(): Promise<void>;
  emitEvent(name: string, payload?: Record<string, unknown>): Promise<void>;
  answerService(requestId: string, result: unknown): void;
  failService(requestId: string, error: PluginProtocolErrorPayload): void;
  /** Makes `session_read` answer with `projection` (`null`: no session). */
  setSession(projection: unknown): void;
  /** Answers `model.request` with `text`. */
  setModelReply(text: string): void;
  /** Pushes one projected session event into a subscription, as the host
   * would after a core event. */
  emitSessionEvent(
    subscription: string,
    event: unknown,
    options?: { cursor?: number; dropped?: number; epoch?: number },
  ): void;
  /** The subscriptions the plugin opened (id → session id). */
  subscriptions(): Readonly<Record<string, string>>;
  /** The merged runtime contributions the plugin registered. */
  contributions(): {
    tools: ToolDefinition[];
    commands: Record<string, unknown>[];
    providers: Record<string, unknown>[];
    hooks: Record<string, unknown>[];
  };
  tools(): string[];
  commands(): string[];
  providers(): string[];
  hooks(): Record<string, unknown>[];
  disposed(): boolean;
}

const DEFAULT_IDENTITY: PluginIdentity = {
  instance: "demo:sha256:test:user:default",
  epoch: 3,
  generation: 7,
  protocol: PLUGIN_PROTOCOL_VERSION,
  servicesProtocol: 1,
  locale: "zh-CN",
  package: { id: "demo", digest: "sha256:test" },
};

const DEFAULT_READY_TIMEOUT_MS = 5_000;
const DEFAULT_CALL_TIMEOUT_MS = 5_000;

/** Creates a host without starting yet: import the backend (or pass `applier`)
 * first, then `start()`. */
export function createTestHost(options: TestHostOptions = {}): TestHost {
  return new TestHostImpl(options);
}

class TestHostImpl implements TestHost {
  private readonly identity_: PluginIdentity;
  private readonly applier: PluginApplier | null;
  private readonly readyTimeoutMs: number;
  private readonly callTimeoutMs: number;
  private readonly outgoing = new PluginSequencer();
  private readonly reportedErrors: string[] = [];
  private readonly sentEnvelopes: PluginEnvelope[] = [];
  private readonly requests: ServiceRequestRecord[] = [];
  private readonly registeredTools = new Map<string, ToolDefinition<any, any>>();
  private readonly registeredCommands = new Map<string, Record<string, unknown>>();
  private readonly registeredProviders = new Map<string, Record<string, unknown>>();
  private readonly registeredHooks = new Map<string, Record<string, unknown>>();
  private readonly pendingCalls = new Map<
    string,
    { resolve(value: unknown): void; reject(error: PluginError): void }
  >();
  private readonly pendingSelections = new Map<
    string,
    { resolve(value: string[]): void; reject(error: PluginError): void }
  >();
  private listener: ((envelope: PluginEnvelope) => void) | null = null;
  private readyResolver: (() => void) | null = null;
  private healthResolver: (() => void) | null = null;
  private session: unknown = undefined;
  private modelReply = "test model answer";
  private readonly openSubscriptions = new Map<string, string>();
  private readonly kv = new Map<string, { value: unknown; revision: number; deleted: boolean }>();
  private readonly subscriptionCursors = new Map<string, number>();
  private subscriptionCounter = 0;
  private started = false;
  private isReady = false;
  private isDisposed = false;
  private requestCounter = 0;
  private healthCounter = 0;

  constructor(options: TestHostOptions) {
    this.identity_ = {
      ...DEFAULT_IDENTITY,
      ...options.identity,
      package: {
        ...DEFAULT_IDENTITY.package,
        ...(options.identity?.package ?? {}),
      },
    };
    this.applier = options.applier ?? null;
    this.readyTimeoutMs = options.readyTimeoutMs ?? DEFAULT_READY_TIMEOUT_MS;
    this.callTimeoutMs = options.callTimeoutMs ?? DEFAULT_CALL_TIMEOUT_MS;
  }

  get ready(): boolean {
    return this.isReady;
  }

  get identity(): PluginIdentity {
    return this.identity_;
  }

  get errors(): readonly string[] {
    return this.reportedErrors;
  }

  get envelopes(): readonly PluginEnvelope[] {
    return this.sentEnvelopes;
  }

  get serviceRequests(): readonly ServiceRequestRecord[] {
    return this.requests;
  }

  async start(): Promise<TestHost> {
    if (this.started) return this;
    this.started = true;
    const applier = this.applier ?? takePendingApplier();
    if (!applier) {
      throw new Error(
        "no plugin applier: import a backend that calls apply(...) or pass { applier }",
      );
    }
    runPlugin(applier, {
      identity: this.identity,
      post: (envelope) => this.onPost(envelope),
      listen: (listener) => {
        this.listener = listener;
        return () => {
          if (this.listener === listener) this.listener = null;
        };
      },
      report: (message) => this.reportedErrors.push(message),
    });
    const ready = new Promise<void>((resolve) => {
      this.readyResolver = resolve;
    });
    this.deliver("lifecycle.start", "lifecycle", {});
    await withTimeout(ready, this.readyTimeoutMs, "the plugin did not become ready");
    return this;
  }

  stop(): void {
    if (this.isDisposed) return;
    this.deliver("lifecycle.stop", "lifecycle", null);
    this.isDisposed = true;
    this.isReady = false;
  }

  async tool<Result = unknown>(
    name: string,
    args: unknown = {},
    options: { sessionId?: string } = {},
  ): Promise<Result> {
    const requestId = `host-tool-${++this.requestCounter}`;
    const answer = new Promise<unknown>((resolve, reject) => {
      this.pendingCalls.set(requestId, { resolve, reject });
    });
    this.deliver(
      "tools.call",
      "tools",
      {
        request_id: requestId,
        session_id: options.sessionId ?? "s-test",
        package: this.identity_.package.id,
        digest: this.identity_.package.digest,
        tool: name,
        args,
      },
      requestId,
    );
    return (await withTimeout(answer, this.callTimeoutMs, `tool \`${name}\` did not answer`)) as Result;
  }

  async select(options: {
    sessionId?: string;
    agentId?: string;
    model?: Record<string, unknown>;
  } = {}): Promise<string[]> {
    const requestId = `host-select-${++this.requestCounter}`;
    const answer = new Promise<string[]>((resolve, reject) => {
      this.pendingSelections.set(requestId, { resolve, reject });
    });
    this.deliver(
      "tools.select",
      "tools",
      {
        request_id: requestId,
        session_id: options.sessionId ?? "s-test",
        package: this.identity_.package.id,
        digest: this.identity_.package.digest,
        tool: "",
        args: {
          request_id: requestId,
          session_id: options.sessionId ?? "s-test",
          package: this.identity_.package.id,
          digest: this.identity_.package.digest,
          agent_id: options.agentId ?? null,
          model: options.model ?? defaultModel(),
        },
      },
      requestId,
    );
    return withTimeout(answer, this.callTimeoutMs, "the tool selection did not answer");
  }

  async command(id: string): Promise<void> {
    this.deliver("host.action", "services", { name: "command.invoke", commandId: id });
    await tick();
  }

  async health(): Promise<void> {
    const token = ++this.healthCounter;
    const answer = new Promise<void>((resolve) => {
      this.healthResolver = resolve;
    });
    this.deliver("lifecycle.health", "lifecycle", { token });
    await withTimeout(answer, this.callTimeoutMs, "the health check did not answer");
  }

  async emitEvent(name: string, payload: Record<string, unknown> = {}): Promise<void> {
    this.deliver("service.event", "services", { name, ...payload });
    await tick();
  }

  answerService(requestId: string, result: unknown): void {
    const request = this.requests.find((entry) => entry.requestId === requestId);
    this.deliver(
      "service.response",
      "services",
      { service: request?.service ?? "", result: { Ok: result } },
      requestId,
    );
  }

  failService(requestId: string, error: PluginProtocolErrorPayload): void {
    const request = this.requests.find((entry) => entry.requestId === requestId);
    this.deliver(
      "service.response",
      "services",
      { service: request?.service ?? "", result: { Err: error } },
      requestId,
    );
  }

  setSession(projection: unknown): void {
    this.session = projection;
  }

  setModelReply(text: string): void {
    this.modelReply = text;
  }

  emitSessionEvent(
    subscription: string,
    event: unknown,
    options: { cursor?: number; dropped?: number; epoch?: number } = {},
  ): void {
    const cursor = options.cursor ?? (this.subscriptionCursors.get(subscription) ?? 0) + 1;
    this.subscriptionCursors.set(subscription, cursor);
    this.deliver("service.event", "services", {
      name: "session.event",
      subscription,
      epoch: options.epoch ?? this.identity_.epoch,
      cursor,
      dropped: options.dropped ?? 0,
      event,
    });
  }

  subscriptions(): Readonly<Record<string, string>> {
    return Object.fromEntries(this.openSubscriptions);
  }

  contributions(): {
    tools: ToolDefinition[];
    commands: Record<string, unknown>[];
    providers: Record<string, unknown>[];
    hooks: Record<string, unknown>[];
  } {
    return {
      tools: [...this.registeredTools.values()],
      commands: [...this.registeredCommands.values()],
      providers: [...this.registeredProviders.values()],
      hooks: [...this.registeredHooks.values()],
    };
  }

  tools(): string[] {
    return [...this.registeredTools.keys()];
  }

  commands(): string[] {
    return [...this.registeredCommands.values()].map((command) => String(command.id));
  }

  providers(): string[] {
    return [...this.registeredProviders.values()].map((provider) => String(provider.id));
  }

  hooks(): Record<string, unknown>[] {
    return [...this.registeredHooks.values()];
  }

  disposed(): boolean {
    return this.isDisposed;
  }

  sendRaw(envelope: PluginEnvelope): void {
    this.listener?.(envelope);
  }

  private onPost(envelope: PluginEnvelope): void {
    this.sentEnvelopes.push(envelope);
    if (!validatePluginEnvelope(envelope)) {
      this.reportedErrors.push("the plugin sent an invalid envelope");
      return;
    }
    if (!isCurrent(envelope, this.identity)) {
      this.reportedErrors.push("the plugin sent an envelope for another instance, epoch or generation");
      return;
    }
    const body = (envelope.body ?? {}) as Record<string, unknown>;
    switch (envelope.kind) {
      case "lifecycle.hello": {
        if (body.uiProtocol !== PLUGIN_PROTOCOL_VERSION) {
          this.reportedErrors.push("the plugin requested an unsupported protocol");
        }
        break;
      }
      case "lifecycle.ready": {
        this.isReady = true;
        this.readyResolver?.();
        this.readyResolver = null;
        break;
      }
      case "lifecycle.failed":
        this.reportedErrors.push(`the plugin failed: ${String(body.message ?? "")}`);
        break;
      case "lifecycle.health":
        this.healthResolver?.();
        this.healthResolver = null;
        break;
      case "contribution.register":
        this.mergeContributions(body);
        break;
      case "contribution.remove":
        this.removeContribution(String(body.id ?? ""));
        break;
      case "tools.result":
        this.settleTool(body, envelope.requestId ?? "");
        break;
      case "service.request":
        this.onServiceRequest(body, envelope.requestId ?? "");
        break;
      case "log.event":
        break;
      default:
        break;
    }
  }

  private settleTool(body: Record<string, unknown>, requestId: string): void {
    const error = body.error as PluginProtocolErrorPayload | undefined;
    const settle = (pending: { resolve(value: unknown): void; reject(error: PluginError): void } | undefined) => {
      if (!pending) return;
      if (error) {
        pending.reject(new PluginError(error.code ?? "internal", error.message ?? "tool failed", Boolean(error.retryable)));
      } else {
        pending.resolve(body.result ?? null);
      }
    };
    const call = this.pendingCalls.get(requestId);
    if (call) {
      this.pendingCalls.delete(requestId);
      settle(call);
      return;
    }
    const selection = this.pendingSelections.get(requestId);
    if (selection) {
      this.pendingSelections.delete(requestId);
      if (error) {
        selection.reject(new PluginError(error.code ?? "internal", error.message ?? "selection failed"));
      } else {
        const selected = body.result;
        if (!Array.isArray(selected) || selected.some((name) => typeof name !== "string")) {
          selection.reject(new PluginError("invalid_contribution", "the selection is not a list of names"));
        } else {
          selection.resolve(selected as string[]);
        }
      }
    }
  }

  private onServiceRequest(body: Record<string, unknown>, requestId: string): void {
    const service = String(body.service ?? "");
    const record: ServiceRequestRecord = {
      requestId,
      service,
      payload: body.payload,
      sessionId: typeof body.sessionId === "string" ? body.sessionId : undefined,
      workspaceId: typeof body.workspaceId === "string" ? body.workspaceId : undefined,
      idempotencyKey:
        typeof body.idempotencyKey === "string" ? body.idempotencyKey : undefined,
    };
    this.requests.push(record);
    if (service === "locale_read") {
      this.answerService(requestId, { locale: this.identity_.locale });
    } else if (service === "session_read" && this.session !== undefined) {
      this.answerService(requestId, { generation: 1, session: this.session });
    } else if (service === "storage_read") {
      this.answerService(requestId, null);
    } else if (service === "storage_list") {
      this.answerService(requestId, {
        keys: [...this.kv.entries()].filter(([, entry]) => !entry.deleted).map(([key, entry]) => ({
          key,
          revision: entry.revision,
          bytes: JSON.stringify(entry.value).length,
        })),
      });
    } else if (service === "storage_key_read") {
      const key = String((body.payload as Record<string, unknown> | undefined)?.key ?? "");
      const entry = this.kv.get(key);
      this.answerService(
        requestId,
        entry
          ? { value: entry.deleted ? null : entry.value, revision: entry.revision }
          : { value: null, revision: 0 },
      );
    } else if (service === "storage_key_write") {
      const payload = (body.payload ?? {}) as Record<string, unknown>;
      const key = String(payload.key ?? "");
      const current = this.kv.get(key)?.revision ?? 0;
      if (payload.expectedRevision !== undefined && payload.expectedRevision !== current) {
        this.failService(requestId, {
          code: "stale_revision",
          message: `the key is at revision ${current}`,
          details: { currentRevision: current },
        });
      } else {
        const revision = current + 1;
        this.kv.set(key, { value: payload.value, revision, deleted: false });
        this.answerService(requestId, { revision });
      }
    } else if (service === "storage_key_delete") {
      const payload = (body.payload ?? {}) as Record<string, unknown>;
      const key = String(payload.key ?? "");
      const entry = this.kv.get(key);
      const current = entry?.revision ?? 0;
      if (payload.expectedRevision !== undefined && payload.expectedRevision !== current) {
        this.failService(requestId, {
          code: "stale_revision",
          message: `the key is at revision ${current}`,
          details: { currentRevision: current },
        });
      } else if (!entry || entry.deleted) {
        this.answerService(requestId, { deleted: false, revision: current });
      } else {
        const revision = current + 1;
        this.kv.set(key, { value: null, revision, deleted: true });
        this.answerService(requestId, { deleted: true, revision });
      }
    } else if (service === "session_subscribe") {
      const sessionId = String(body.sessionId ?? record.sessionId ?? "");
      const subscription = `sub-${++this.subscriptionCounter}`;
      this.openSubscriptions.set(subscription, sessionId);
      this.subscriptionCursors.set(subscription, 0);
      this.answerService(requestId, {
        subscription,
        sessionId,
        epoch: this.identity_.epoch,
        cursor: 0,
        dropped: 0,
      });
    } else if (service === "session_unsubscribe") {
      const subscription = String((body.payload as Record<string, unknown> | undefined)?.subscription ?? "");
      this.openSubscriptions.delete(subscription);
      this.answerService(requestId, null);
    } else if (service === "session_create") {
      this.answerService(requestId, { sessionId: "s-created" });
    } else if (service === "session_send") {
      this.answerService(requestId, { accepted: true });
    } else if (service === "session_interrupt" || service === "session_unsubscribe") {
      this.answerService(requestId, null);
    } else if (service === "session_wait") {
      this.answerService(requestId, { status: "completed" });
    } else if (service === "model_request") {
      this.answerService(requestId, {
        text: this.modelReply,
        usage: { inputTokens: 1, outputTokens: 1 },
      });
    } else if (service === "agent_list") {
      this.answerService(requestId, {
        profiles: [
          { name: "explore", description: "Read-only search", source: "built_in" },
          { name: "general-purpose", description: "General work", source: "built_in" },
        ],
      });
    } else if (service === "agent_start") {
      this.answerService(requestId, {
        agentId: "agent-1",
        taskId: "agent-1",
        profile: String(body.profile ?? "explore"),
        background: Boolean(body.background ?? true),
        status: "running",
      });
    } else if (service === "agent_wait") {
      this.answerService(requestId, {
        agentId: String(body.agentId ?? "agent-1"),
        status: "completed",
        summary: "done",
      });
    } else if (service === "agent_read") {
      this.answerService(requestId, {
        agentId: String(body.agentId ?? "agent-1"),
        title: "agent",
        running: false,
        items: [],
      });
    } else if (service === "agent_send" || service === "agent_stop") {
      this.answerService(requestId, null);
    } else if (service === "git_status") {
      this.answerService(requestId, { unstaged: [], staged: [] });
    } else if (service === "git_diff") {
      this.answerService(requestId, { diff: "", note: null });
    } else if (service === "git_log") {
      this.answerService(requestId, { commits: [] });
    } else if (service === "git_show") {
      this.answerService(requestId, { text: "", truncated: false });
    } else if (service === "git_branches") {
      this.answerService(requestId, { current: null, branches: [] });
    } else if (service === "workspace_file_read") {
      const payload = (body.payload ?? {}) as Record<string, unknown>;
      const path = String(payload.path ?? "");
      this.answerService(requestId, {
        path,
        content: "",
        startLine: payload.startLine ?? 1,
        endLine: payload.endLine ?? 1,
        totalLines: 1,
        size: 0,
        truncated: false,
      });
    }
  }

  private mergeContributions(body: Record<string, unknown>): void {
    for (const tool of (body.tools ?? []) as ToolDefinition[]) {
      if (tool && typeof tool.name === "string") this.registeredTools.set(tool.name, tool);
    }
    for (const command of (body.commands ?? []) as Record<string, unknown>[]) {
      if (command && typeof command.id === "string") this.registeredCommands.set(command.id, command);
    }
    for (const provider of (body.providers ?? []) as Record<string, unknown>[]) {
      if (provider && typeof provider.id === "string") this.registeredProviders.set(provider.id, provider);
    }
    for (const hook of (body.hooks ?? []) as Record<string, unknown>[]) {
      const id = typeof hook.id === "string" ? hook.id : String(hook.command ?? "");
      this.registeredHooks.set(id, hook);
    }
  }

  private removeContribution(id: string): void {
    this.registeredTools.delete(id);
    this.registeredCommands.delete(id);
    this.registeredProviders.delete(id);
    this.registeredHooks.delete(id);
    for (const [key, hook] of this.registeredHooks) {
      if (hook.command === id) this.registeredHooks.delete(key);
    }
  }

  private deliver(
    kind: PluginMessageKind,
    stream: PluginStream,
    body: unknown,
    requestId?: string,
  ): void {
    const envelope: PluginEnvelope = {
      protocol: PLUGIN_PROTOCOL_VERSION,
      kind,
      instance: this.identity_.instance,
      epoch: this.identity_.epoch,
      stream,
      sequence: this.outgoing.next(stream),
      generation: this.identity_.generation,
      body: body ?? null,
    };
    if (requestId) envelope.requestId = requestId;
    this.listener?.(envelope);
  }
}

function defaultModel(): Record<string, unknown> {
  return {
    provider_name: "test",
    model_id: "test-model",
    api_format: "open_ai_chat",
    context_window: 128_000,
    max_output_tokens: 4096,
    capabilities: {
      native_web_search: false,
      input_image: true,
      input_pdf: true,
      structured_output: true,
      strict_tools: true,
    },
  };
}

function tick(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

function withTimeout<T>(promise: Promise<T>, ms: number, message: string): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(message)), ms);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (error) => {
        clearTimeout(timer);
        reject(error);
      },
    );
  });
}

/** Exported for tests that assert a structured failure without a host. */
export function rejectedAsPluginError(error: unknown): PluginError {
  return asPluginError(error);
}
