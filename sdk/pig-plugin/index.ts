// The `pig/plugin` SDK. A backend only states what it offers and what it does;
// the SDK owns the envelope protocol:
//
//   - instance, epoch and generation come from the host's launch parameters;
//   - one sequence counter per stream grows for every message the plugin sends;
//   - stale or foreign inbound envelopes (another instance, epoch, generation or
//     sequence) are dropped before they reach a handler;
//   - every service request and tool call gets a unique request id that is
//     settled exactly once, even after a timeout, a cancellation or a dispose.
//
// ```ts
// import { apply } from "pig/plugin";
//
// apply((ctx) => {
//   ctx.tools.register({
//     name: "DocSearch",
//     description: "Search the documentation",
//     parameters: { type: "object", properties: { query: { type: "string" } }, required: ["query"] },
//     readOnly: true,
//     handler: ({ query }) => ({ hits: [query] }),
//   });
// });
// ```

import { host } from "nsh";

import {
  asPluginError,
  isCurrent,
  PLUGIN_PROTOCOL_VERSION,
  PluginError,
  PluginSequencer,
  readPluginIdentity,
  validatePluginEnvelope,
  type PluginEnvelope,
  type PluginErrorCode,
  type PluginIdentity,
  type PluginMessageKind,
  type PluginProtocolErrorPayload,
  type PluginStream,
} from "./protocol";

export {
  PLUGIN_PROTOCOL_VERSION,
  PLUGIN_SERVICES_PROTOCOL_VERSION,
  PluginError,
  PluginSequencer,
  readPluginIdentity,
  validatePluginEnvelope,
} from "./protocol";
export type {
  PluginEnvelope,
  PluginErrorCode,
  PluginIdentity,
  PluginMessageKind,
  PluginProtocolErrorPayload,
  PluginStream,
} from "./protocol";
export type Disposer = () => void;

// --- Tools -------------------------------------------------------------------

export type ToolApproval = "on_request" | "always" | "never";
export type ToolAvailability = "static" | "plugin";

/** Cancellation of one call (the host sends `tools.cancel` when the turn stops
 * or the call's deadline passes). */
export interface ToolSignal {
  readonly cancelled: boolean;
  onCancel(listener: () => void): Disposer;
}

export interface ToolCallInfo {
  readonly requestId: string;
  readonly sessionId: string;
  readonly package: { id: string; digest: string };
  readonly signal: ToolSignal;
}

export interface ToolDefinition<Args = unknown, Result = unknown> {
  name: string;
  description: string;
  /** JSON Schema of the arguments; the host validates every call. */
  parameters?: Record<string, unknown>;
  /** JSON Schema of the result; the host validates it before the model sees it. */
  outputSchema?: Record<string, unknown>;
  readOnly?: boolean;
  availability?: ToolAvailability;
  idempotent?: boolean;
  approval?: ToolApproval;
  /** One call's deadline in ms (host default 60000, host max 600000). */
  timeoutMs?: number;
  /** Calls of this tool allowed in flight at once (default 8, max 32). */
  maxConcurrency?: number;
  /** Inline result budget in bytes (default 32 KiB); larger results are
   * written to the session's tool-results directory and previewed. */
  maxOutputBytes?: number;
  handler: (args: Args, call: ToolCallInfo) => Result | Promise<Result>;
}

/** Tool-selection input: the model and the conversation the call belongs to. */
export interface ToolSelectionContext {
  sessionId: string;
  agentId?: string;
  model: PluginModelContext;
  /** The tools this package declared with `availability: "plugin"`. */
  tools: ToolDefinition[];
}

export type ToolSelector = (context: ToolSelectionContext) => string[] | Promise<string[]>;

export interface PluginModelCapabilities {
  nativeWebSearch: boolean;
  inputImage: boolean;
  inputPdf: boolean;
  structuredOutput: boolean;
  strictTools: boolean;
}

export interface PluginModelContext {
  providerName: string;
  modelId: string;
  apiFormat: string;
  contextWindow: number;
  maxOutputTokens: number;
  capabilities: PluginModelCapabilities;
}

// --- Commands ----------------------------------------------------------------

export interface CommandDefinition {
  id: string;
  title: string | Record<string, string>;
  order?: number;
}

export type CommandHandler = (call: { commandId: string }) => unknown | Promise<unknown>;

// --- Hooks -------------------------------------------------------------------

export type HookEvent =
  | "session_start"
  | "user_prompt_submit"
  | "pre_tool_use"
  | "permission_request"
  | "post_tool_use"
  | "post_tool_use_failure"
  | "stop";

/** A hook contribution, executed by the host's ordinary hook runner with the
 * user's master switch, timeout, output limits and redaction in force. */
export interface HookDefinition {
  event: HookEvent;
  /** `command` (a shell string, default) or `process` (argv, no shell). */
  kind?: "command" | "process";
  command: string;
  args?: string[];
  shell?: string;
  matcher?: string;
  timeoutMs?: number;
  /** Fire-and-forget: output and decisions are ignored. */
  background?: boolean;
  statusMessage?: string;
  enabled?: boolean;
}

// --- Services ----------------------------------------------------------------

export type PluginService =
  | "storage_read"
  | "storage_write"
  | "config_read"
  | "config_write"
  | "http_request"
  | "credential_set"
  | "credential_clear"
  | "session_read"
  | "workspace_read"
  | "navigation_open"
  | "composer_submit"
  | "resource_read"
  | "locale_read";

export interface ServiceRequestOptions {
  sessionId?: string;
  workspaceId?: string;
  timeoutMs?: number;
}

export interface PluginHttpRequest {
  origin: string;
  method: string;
  query?: Record<string, string>;
  headers?: Record<string, string>;
  body?: unknown;
  credential?: { credential: string; header?: string; jsonPointer?: string; prefix?: string };
}

export interface PluginHttpResponse {
  status: number;
  body: string;
}

/** What each service expects as its payload. */
export interface ServicePayloads {
  storage_read: null;
  storage_write: unknown;
  config_read: { path: string };
  config_write: { path: string; value: unknown };
  http_request: PluginHttpRequest;
  credential_set: { credential: string; secret: string };
  credential_clear: { credential: string };
  session_read: null;
  workspace_read: null;
  navigation_open: { pageId: string };
  composer_submit: { text: string };
  resource_read: { uri: string };
  locale_read: null;
}

/** What each service answers with. */
export interface ServiceResults {
  storage_read: unknown;
  storage_write: null;
  config_read: unknown;
  config_write: unknown;
  http_request: PluginHttpResponse;
  credential_set: null;
  credential_clear: null;
  session_read: { generation: number; session: SessionProjection | null };
  workspace_read: {
    generation: number;
    scope: unknown;
    workspaces: WorkspaceProjection[];
  };
  navigation_open: null;
  composer_submit: null;
  resource_read: { mime: string; bytes: number[] };
  locale_read: { locale: string };
}

export interface ContextUsage {
  used: number;
  total: number;
  cacheReadTotal: number;
  inputTotal: number;
}

export interface SessionStats {
  running: boolean;
  outputRate: number;
  turns: number;
  steps: number;
  output: number;
  decodeMs: number;
  apiMs: number;
  toolMs: number;
  ttftMs: number;
  ttftSteps: number;
  usage?: ContextUsage | null;
}

export interface SessionProjection {
  id: string;
  cwd: string;
  stats: SessionStats;
  turns: unknown[];
}

export interface WorkspaceProjection {
  path: string;
  name: string;
  current: boolean;
}

export interface PluginConfigApi {
  read<Value = unknown>(path: string): Promise<Value>;
  write<Value = unknown>(path: string, value: unknown): Promise<Value>;
}

export interface PluginStorageApi {
  /** `null` when nothing was written yet. */
  read<Value = unknown>(): Promise<Value | null>;
  write(value: unknown): Promise<void>;
}

export interface PluginSessionApi {
  /** The host answers `session_read` with `{generation, session}`; this
   * unwraps the projection (`null` when there is no such session). */
  read<Value = unknown>(options?: { sessionId?: string }): Promise<Value | null>;
}

export interface PluginServices {
  request<Service extends PluginService>(
    service: Service,
    payload?: ServicePayloads[Service],
    options?: ServiceRequestOptions,
  ): Promise<ServiceResults[Service]>;
  request<Result = unknown>(
    service: PluginService,
    payload?: unknown,
    options?: ServiceRequestOptions,
  ): Promise<Result>;
  /** Host events addressed to the plugin (`locale.changed`, ...). */
  on(name: string, listener: (payload: Record<string, unknown>) => void): Disposer;
  readonly config: PluginConfigApi;
  readonly storage: PluginStorageApi;
  readonly session: PluginSessionApi;
}

// --- Context -----------------------------------------------------------------

export interface PluginLogger {
  debug(message: string, fields?: Record<string, unknown>): void;
  info(message: string, fields?: Record<string, unknown>): void;
  warn(message: string, fields?: Record<string, unknown>): void;
  error(message: string, fields?: Record<string, unknown>): void;
}

export interface PluginContext {
  readonly identity: PluginIdentity;
  readonly package: { id: string; digest: string };
  readonly locale: string;
  readonly tools: {
    register<Args = unknown, Result = unknown>(definition: ToolDefinition<Args, Result>): Disposer;
    /** Replaces the default "every dynamic tool" selection. */
    select(selector: ToolSelector): Disposer;
  };
  readonly commands: {
    register(definition: CommandDefinition, handler: CommandHandler): Disposer;
  };
  readonly hooks: {
    register(definition: HookDefinition): Disposer;
  };
  readonly services: PluginServices;
  /** Alias of `services.on` for host events. */
  readonly events: {
    on(name: string, listener: (payload: Record<string, unknown>) => void): Disposer;
  };
  readonly log: PluginLogger;
  /** Runs after the host accepts this instance; requests made before that are
   * queued until then. */
  onReady(listener: () => void): Disposer;
  /** Registers a teardown step (run in reverse order on stop). */
  dispose(disposer: Disposer): void;
}

export type PluginApplier = (context: PluginContext) => void | Disposer | Promise<void | Disposer>;

/** The host side the runtime speaks to: NSH's `nsh/host` in the app, an
 * in-memory one in tests. */
export interface PluginHostBridge {
  readonly identity: PluginIdentity;
  post(envelope: PluginEnvelope): void;
  listen(listener: (envelope: PluginEnvelope) => void): Disposer;
  /** Optional: diagnostics the runtime could not turn into envelopes. */
  report?(message: string): void;
}

// --- Registration & runtime --------------------------------------------------

let pendingApplier: PluginApplier | null = null;

/**
 * Declares this backend's apply function. In an embedded host the SDK starts
 * immediately (the host's launch parameters say who this instance is); when
 * the module is imported outside a host — a test importing the backend — the
 * applier is kept for [`createTestHost`].
 */
export function apply(applier: PluginApplier): Disposer {
  pendingApplier = applier;
  let running: { dispose: Disposer } | null = null;
  try {
    const identity = readPluginIdentity(hostParams());
    if (identity) running = runPlugin(applier, nshBridge(identity));
  } catch {
    // No `nsh/host` (tests, a plain Node import): nothing to boot.
  }
  return () => {
    if (pendingApplier === applier) pendingApplier = null;
    running?.dispose();
  };
}

/** @internal The test host claims the last `apply` declaration. */
export function takePendingApplier(): PluginApplier | null {
  const applier = pendingApplier;
  pendingApplier = null;
  return applier;
}

/** Starts an applier against any bridge. Exposed for tests and custom hosts. */
export function runPlugin(applier: PluginApplier, bridge: PluginHostBridge): { dispose: Disposer } {
  const runtime = new PluginRuntime(applier, bridge);
  runtime.start();
  return { dispose: () => runtime.dispose() };
}

function hostParams(): unknown {
  try {
    if (typeof host === "undefined" || !host.available()) return undefined;
    return host.params;
  } catch {
    return undefined;
  }
}

function nshBridge(identity: PluginIdentity): PluginHostBridge {
  return {
    identity,
    post(envelope) {
      host.post("plugin.protocol", envelope);
    },
    listen(listener) {
      return host.on("plugin.protocol", (data) => listener(data as unknown as PluginEnvelope));
    },
  };
}

class ToolSignalImpl implements ToolSignal {
  private listeners = new Set<() => void>();
  private wasCancelled = false;

  get cancelled(): boolean {
    return this.wasCancelled;
  }

  onCancel(listener: () => void): Disposer {
    if (this.wasCancelled) {
      listener();
      return () => {};
    }
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  cancel(): void {
    if (this.wasCancelled) return;
    this.wasCancelled = true;
    const listeners = [...this.listeners];
    this.listeners.clear();
    for (const listener of listeners) {
      try {
        listener();
      } catch {
        // A cancellation handler cannot fail the call it is cancelling.
      }
    }
  }
}

interface PendingService {
  service: PluginService;
  resolve(value: unknown): void;
  reject(error: PluginError): void;
  timer: ReturnType<typeof setTimeout> | null;
}

interface PendingSend {
  send: () => void;
}

const DEFAULT_SERVICE_TIMEOUT_MS = 30_000;

class PluginRuntime {
  private readonly identity: PluginIdentity;
  private readonly bridge: PluginHostBridge;
  private readonly outgoing = new PluginSequencer();
  private readonly incoming = new PluginSequencer();
  private readonly tools = new Map<string, ToolDefinition<any, any>>();
  private readonly selectors: ToolSelector[] = [];
  private readonly commands = new Map<string, { definition: CommandDefinition; handler: CommandHandler }>();
  private readonly hooks = new Map<string, HookDefinition>();
  private readonly eventListeners = new Map<string, Set<(payload: Record<string, unknown>) => void>>();
  private readonly readyListeners = new Set<() => void>();
  private readonly disposers: Disposer[] = [];
  private readonly pendingServices = new Map<string, PendingService>();
  private readonly queuedServiceSends: PendingSend[] = [];
  private readonly activeCalls = new Map<string, ToolSignalImpl>();
  private readonly stopListening: Disposer;
  private readonly applier: PluginApplier;
  private initialized = false;
  private ready = false;
  private disposed = false;
  private requestCounter = 0;
  private hookCounter = 0;
  private readonly context: PluginContext;

  constructor(applier: PluginApplier, bridge: PluginHostBridge) {
    this.identity = bridge.identity;
    this.bridge = bridge;
    this.applier = applier;
    this.stopListening = bridge.listen((envelope) => this.receive(envelope));
    this.context = this.buildContext();
  }

  start(): void {
    this.send("lifecycle.hello", "lifecycle", {
      uiProtocol: PLUGIN_PROTOCOL_VERSION,
      servicesProtocol: this.identity.servicesProtocol,
    });
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.stopListening();
    for (const signal of this.activeCalls.values()) signal.cancel();
    this.activeCalls.clear();
    const error = new PluginError("not_active", "the plugin instance stopped");
    for (const pending of this.pendingServices.values()) {
      if (pending.timer !== null) clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.pendingServices.clear();
    this.queuedServiceSends.length = 0;
    const disposers = [...this.disposers].reverse();
    this.disposers.length = 0;
    for (const disposer of disposers) {
      try {
        disposer();
      } catch (failure) {
        this.report(`a plugin disposer failed: ${asPluginError(failure).message}`);
      }
    }
  }

  private buildContext(): PluginContext {
    const runtime = this;
    const on = (name: string, listener: (payload: Record<string, unknown>) => void): Disposer => {
      let listeners = runtime.eventListeners.get(name);
      if (!listeners) {
        listeners = new Set();
        runtime.eventListeners.set(name, listeners);
      }
      listeners.add(listener);
      return () => listeners?.delete(listener);
    };
    const request = (service: PluginService, payload: unknown, options?: ServiceRequestOptions) =>
      runtime.requestService(service, payload, options);
    const services: PluginServices = {
      request: request as PluginServices["request"],
      on,
      config: {
        read: <Value = unknown>(path: string) => runtime.requestService<Value>("config_read", { path }),
        write: <Value = unknown>(path: string, value: unknown) =>
          runtime.requestService<Value>("config_write", { path, value }),
      },
      storage: {
        read: <Value = unknown>() => runtime.requestService<Value | null>("storage_read", null),
        write: async (value) => {
          await runtime.requestService("storage_write", value);
        },
      },
      session: {
        read: async <Value = unknown>(options?: { sessionId?: string }) => {
          const result = await runtime.requestService<{ session?: Value | null }>(
            "session_read",
            null,
            { sessionId: options?.sessionId },
          );
          return result?.session ?? null;
        },
      },
    };
    const context: PluginContext = {
      identity: this.identity,
      package: this.identity.package,
      locale: this.identity.locale,
      tools: {
        register: (definition) => runtime.registerTool(definition as ToolDefinition<any, any>),
        select: (selector) => {
          runtime.selectors.push(selector);
          return () => {
            const index = runtime.selectors.indexOf(selector);
            if (index >= 0) runtime.selectors.splice(index, 1);
          };
        },
      },
      commands: {
        register: (definition, handler) => runtime.registerCommand(definition, handler),
      },
      hooks: {
        register: (definition) => runtime.registerHook(definition),
      },
      services,
      events: { on },
      log: {
        debug: (message, fields) => runtime.log("debug", message, fields),
        info: (message, fields) => runtime.log("info", message, fields),
        warn: (message, fields) => runtime.log("warn", message, fields),
        error: (message, fields) => runtime.log("error", message, fields),
      },
      onReady: (listener) => {
        if (runtime.ready) {
          listener();
          return () => {};
        }
        runtime.readyListeners.add(listener);
        return () => runtime.readyListeners.delete(listener);
      },
      dispose: (disposer) => {
        runtime.disposers.push(disposer);
      },
    };
    return context;
  }

  private receive(raw: unknown): void {
    if (this.disposed) return;
    if (!validatePluginEnvelope(raw)) {
      this.report("the host sent an invalid plugin envelope");
      return;
    }
    const envelope = raw;
    if (!isCurrent(envelope, this.identity)) return;
    if (!this.incoming.accept(envelope.stream, envelope.sequence)) return;
    const body = (envelope.body ?? {}) as Record<string, unknown>;
    switch (envelope.kind) {
      case "lifecycle.start":
        void this.initialize();
        break;
      case "lifecycle.health":
        this.send("lifecycle.health", "lifecycle", { token: body.token ?? 0 });
        break;
      case "lifecycle.stop":
        this.dispose();
        break;
      case "tools.call":
        void this.callTool(body, envelope.requestId ?? null);
        break;
      case "tools.select":
        void this.selectTools(body, envelope.requestId ?? null);
        break;
      case "tools.cancel": {
        const requestId = String(body.request_id ?? "");
        this.activeCalls.get(requestId)?.cancel();
        break;
      }
      case "host.action":
        if (body.name === "command.invoke") void this.invokeCommand(String(body.commandId ?? ""));
        break;
      case "service.response":
        this.settleService(body, envelope.requestId ?? "");
        break;
      case "service.event":
        this.dispatchEvent(body);
        break;
      default:
        break;
    }
  }

  private async initialize(): Promise<void> {
    if (this.initialized || this.disposed) return;
    this.initialized = true;
    try {
      const applier = this.applier;
      const disposer = await applier(this.context);
      if (typeof disposer === "function") this.disposers.push(disposer);
      if (this.disposed) return;
      this.flushContributions();
      this.send("lifecycle.ready", "lifecycle", { accepted: true });
      this.ready = true;
      for (const queued of this.queuedServiceSends.splice(0)) queued.send();
      for (const listener of [...this.readyListeners]) {
        this.readyListeners.delete(listener);
        try {
          listener();
        } catch (failure) {
          this.report(`a ready listener failed: ${asPluginError(failure).message}`);
        }
      }
    } catch (failure) {
      const error = asPluginError(failure);
      this.send("lifecycle.failed", "lifecycle", { message: error.message });
      this.dispose();
    }
  }

  private registerTool(definition: ToolDefinition<any, any>): Disposer {
    this.tools.set(definition.name, definition);
    if (this.initialized) this.flushContributions();
    return () => {
      this.tools.delete(definition.name);
      if (this.initialized) this.removeContribution(definition.name);
    };
  }

  private registerCommand(definition: CommandDefinition, handler: CommandHandler): Disposer {
    this.commands.set(definition.id, { definition, handler });
    if (this.initialized) this.flushContributions();
    return () => {
      this.commands.delete(definition.id);
      if (this.initialized) this.removeContribution(definition.id);
    };
  }

  private registerHook(definition: HookDefinition): Disposer {
    const id = `sdk-hook-${++this.hookCounter}`;
    this.hooks.set(id, definition);
    if (this.initialized) this.flushContributions();
    return () => {
      this.hooks.delete(id);
      if (this.initialized) this.removeContribution(id);
    };
  }

  private flushContributions(): void {
    const tools = [...this.tools.values()].map(toolContribution);
    const commands = [...this.commands.values()].map(({ definition }) => commandContribution(definition));
    const hooks = [...this.hooks.entries()].map(([id, definition]) => hookContribution(id, definition));
    if (tools.length === 0 && commands.length === 0 && hooks.length === 0) return;
    const contributions: Record<string, unknown> = {};
    if (tools.length > 0) contributions.tools = tools;
    if (commands.length > 0) contributions.commands = commands;
    if (hooks.length > 0) contributions.hooks = hooks;
    this.send("contribution.register", "contributions", contributions);
  }

  private removeContribution(id: string): void {
    this.send("contribution.remove", "contributions", { id });
  }

  private async callTool(body: Record<string, unknown>, requestIdFromEnvelope: string | null): Promise<void> {
    const requestId = String(body.request_id ?? requestIdFromEnvelope ?? "");
    const name = String(body.tool ?? "");
    const definition = this.tools.get(name);
    if (!definition) {
      this.sendToolError(requestId, new PluginError("not_found", `unknown plugin tool \`${name}\``));
      return;
    }
    const signal = new ToolSignalImpl();
    this.activeCalls.set(requestId, signal);
    try {
      const result = await definition.handler(body.args ?? {}, {
        requestId,
        sessionId: String(body.session_id ?? ""),
        package: this.identity.package,
        signal,
      });
      if (signal.cancelled) return;
      if (this.disposed) return;
      this.send("tools.result", "tools", { request_id: requestId, result: jsonSafe(result) }, requestId);
    } catch (failure) {
      if (signal.cancelled || this.disposed) return;
      this.sendToolError(requestId, asPluginError(failure));
    } finally {
      this.activeCalls.delete(requestId);
    }
  }

  private async selectTools(body: Record<string, unknown>, requestIdFromEnvelope: string | null): Promise<void> {
    const requestId = String(body.request_id ?? requestIdFromEnvelope ?? "");
    const query = (body.args ?? {}) as Record<string, unknown>;
    const dynamic = [...this.tools.values()].filter(
      (tool) => (tool.availability ?? "static") === "plugin",
    );
    try {
      const selected =
        this.selectors.length > 0
          ? await this.selectors[0]({
              sessionId: String(query.session_id ?? body.session_id ?? ""),
              agentId: typeof query.agent_id === "string" ? query.agent_id : undefined,
              model: modelContext(query.model),
              tools: dynamic,
            })
          : dynamic.map((tool) => tool.name);
      const declared = new Set(dynamic.map((tool) => tool.name));
      const seen = new Set<string>();
      for (const name of selected) {
        if (!declared.has(name)) throw new PluginError("invalid_operation", `tool \`${name}\` is not dynamically declared`);
        if (seen.has(name)) throw new PluginError("invalid_operation", `tool \`${name}\` was selected more than once`);
        seen.add(name);
      }
      this.send("tools.result", "tools", { request_id: requestId, result: selected }, requestId);
    } catch (failure) {
      this.sendToolError(requestId, asPluginError(failure));
    }
  }

  private async invokeCommand(commandId: string): Promise<void> {
    const command = this.commands.get(commandId);
    if (!command) return;
    try {
      await command.handler({ commandId });
    } catch (failure) {
      this.log("error", "a plugin command failed", {
        command: commandId,
        code: asPluginError(failure).code,
        detail: asPluginError(failure).message,
      });
    }
  }

  private requestService<Result = unknown>(
    service: PluginService,
    payload: unknown,
    options: ServiceRequestOptions = {},
  ): Promise<Result> {
    if (this.disposed) {
      return Promise.reject(new PluginError("not_active", "the plugin instance stopped"));
    }
    const requestId = `${this.identity.instance}#svc-${++this.requestCounter}`;
    return new Promise<Result>((resolve, reject) => {
      const pending: PendingService = {
        service,
        resolve: (value) => resolve(value as Result),
        reject,
        timer: null,
      };
      this.pendingServices.set(requestId, pending);
      const send = () => {
        if (this.disposed || !this.pendingServices.has(requestId)) return;
        pending.timer = setTimeout(() => {
          this.pendingServices.delete(requestId);
          reject(new PluginError("timeout", `the ${service} service did not answer in time`, true));
        }, options.timeoutMs ?? DEFAULT_SERVICE_TIMEOUT_MS);
        try {
          this.send(
            "service.request",
            "services",
            {
              service,
              payload: payload ?? null,
              sessionId: options.sessionId ?? null,
              workspaceId: options.workspaceId ?? null,
            },
            requestId,
          );
        } catch (failure) {
          this.pendingServices.delete(requestId);
          if (pending.timer !== null) clearTimeout(pending.timer);
          reject(asPluginError(failure));
        }
      };
      if (this.ready) send();
      else this.queuedServiceSends.push({ send });
    });
  }

  private settleService(body: Record<string, unknown>, requestId: string): void {
    // The host answers with the request id on the envelope; the body carries
    // the service and its `Result`.
    const pending = this.pendingServices.get(requestId);
    if (!pending) return;
    this.pendingServices.delete(requestId);
    if (pending.timer !== null) clearTimeout(pending.timer);
    const result = body.result as Record<string, unknown> | undefined;
    if (result && typeof result === "object" && "Err" in result) {
      const error = (result.Err ?? {}) as PluginProtocolErrorPayload;
      pending.reject(new PluginError(error.code ?? "internal", error.message ?? `${pending.service} failed`, Boolean(error.retryable)));
      return;
    }
    pending.resolve(result && typeof result === "object" && "Ok" in result ? result.Ok : (result ?? null));
  }

  private dispatchEvent(body: Record<string, unknown>): void {
    const name = String(body.name ?? "");
    const listeners = this.eventListeners.get(name);
    if (!listeners) return;
    for (const listener of [...listeners]) {
      try {
        listener(body);
      } catch (failure) {
        this.report(`an event listener failed: ${asPluginError(failure).message}`);
      }
    }
  }

  private sendToolError(requestId: string, error: PluginError): void {
    this.send(
      "tools.result",
      "tools",
      { request_id: requestId, error: error.toJSON() },
      requestId,
    );
  }

  private log(level: string, message: string, fields?: Record<string, unknown>): void {
    this.send("log.event", "logs", { level, message, ...(fields ?? {}) });
  }

  private send(
    kind: PluginMessageKind,
    stream: PluginStream,
    body: Record<string, unknown> | unknown,
    requestId?: string,
  ): void {
    if (this.disposed) return;
    const envelope: PluginEnvelope = {
      protocol: PLUGIN_PROTOCOL_VERSION,
      kind,
      instance: this.identity.instance,
      epoch: this.identity.epoch,
      stream,
      sequence: this.outgoing.next(stream),
      generation: this.identity.generation,
      body: body ?? null,
    };
    if (requestId) envelope.requestId = requestId;
    try {
      this.bridge.post(envelope);
    } catch (failure) {
      this.report(`could not send ${kind}: ${asPluginError(failure).message}`);
    }
  }

  private report(message: string): void {
    if (this.bridge.report) this.bridge.report(message);
  }
}

function toolContribution(definition: ToolDefinition<any, any>): Record<string, unknown> {
  const contribution: Record<string, unknown> = {
    name: definition.name,
    description: definition.description,
    parameters: definition.parameters ?? { type: "object" },
    read_only: definition.readOnly ?? false,
    availability: definition.availability ?? "static",
    idempotent: definition.idempotent ?? false,
    approval: definition.approval ?? "on_request",
  };
  if (definition.outputSchema) contribution.output_schema = definition.outputSchema;
  if (definition.timeoutMs !== undefined) contribution.timeout_ms = definition.timeoutMs;
  if (definition.maxConcurrency !== undefined) contribution.max_concurrency = definition.maxConcurrency;
  if (definition.maxOutputBytes !== undefined) contribution.max_output_bytes = definition.maxOutputBytes;
  return contribution;
}

function commandContribution(definition: CommandDefinition): Record<string, unknown> {
  return {
    id: definition.id,
    title: typeof definition.title === "string" ? { en: definition.title } : definition.title,
    order: definition.order ?? 0,
    enabled: true,
  };
}

function hookContribution(id: string, definition: HookDefinition): Record<string, unknown> {
  const contribution: Record<string, unknown> = {
    id,
    event: definition.event,
    command: definition.command,
    enabled: definition.enabled ?? true,
  };
  if (definition.kind && definition.kind !== "command") contribution.type = definition.kind;
  if (definition.args && definition.args.length > 0) contribution.args = definition.args;
  if (definition.shell) contribution.shell = definition.shell;
  if (definition.matcher) contribution.matcher = definition.matcher;
  if (definition.timeoutMs !== undefined) contribution.timeout_ms = definition.timeoutMs;
  if (definition.background) contribution.async = true;
  if (definition.statusMessage) contribution.status_message = definition.statusMessage;
  return contribution;
}

/** JSON values only cross the host channel; fail loudly instead of sending a
 * silently mangled result. */
function jsonSafe(value: unknown): unknown {
  if (value === undefined) return null;
  try {
    return JSON.parse(JSON.stringify(value));
  } catch {
    throw new PluginError("invalid_output", "the tool result is not JSON-serializable");
  }
}

function modelContext(value: unknown): PluginModelContext {
  const model = (value ?? {}) as Record<string, unknown>;
  const capabilities = (model.capabilities ?? {}) as Record<string, unknown>;
  return {
    providerName: String(model.provider_name ?? model.providerName ?? ""),
    modelId: String(model.model_id ?? model.modelId ?? ""),
    apiFormat: String(model.api_format ?? model.apiFormat ?? ""),
    contextWindow: Number(model.context_window ?? model.contextWindow ?? 0),
    maxOutputTokens: Number(model.max_output_tokens ?? model.maxOutputTokens ?? 0),
    capabilities: {
      nativeWebSearch: Boolean(capabilities.native_web_search ?? capabilities.nativeWebSearch),
      inputImage: Boolean(capabilities.input_image ?? capabilities.inputImage),
      inputPdf: Boolean(capabilities.input_pdf ?? capabilities.inputPdf),
      structuredOutput: Boolean(capabilities.structured_output ?? capabilities.structuredOutput),
      strictTools: Boolean(capabilities.strict_tools ?? capabilities.strictTools),
    },
  };
}
