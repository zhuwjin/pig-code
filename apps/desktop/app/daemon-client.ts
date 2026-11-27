import fs from "fs/promises";
import os from "os";
import process, { type ChildProcess } from "process";
import websocket, { type WebSocketConnection } from "websocket";

export interface DaemonEndpoint {
  protocol: number;
  address: string;
  token: string;
  pid: number;
}

export interface EventCursor {
  epoch: string;
  sequence: number;
}

export interface DaemonInfo {
  cwd: string;
  data_dir: string;
  protocol: number;
  pid: number;
  selftest: boolean;
}

export interface DirectoryEntry {
  name: string;
  path: string;
  is_dir: boolean;
}

export interface DirectoryListing {
  path: string;
  parent: string | null;
  entries: DirectoryEntry[];
  roots: string[];
}

export interface TerminalInfo {
  terminal_id: string;
  shell: string;
  exited?: boolean;
}

export interface RelayConnectionStatus {
  configured: boolean;
  connected: boolean;
  relay_url?: string | null;
  user_id?: string | null;
  username?: string | null;
  device_id?: string | null;
  device_name?: string | null;
  session_token?: string | null;
  last_error?: string | null;
}

export interface RelayDeviceInfo {
  device_id: string;
  name: string;
  os: string;
  arch: string;
  online: boolean;
  last_seen_ms: number;
  workspaces: string[];
  daemon_protocol: number;
}

export interface DeviceCodeStartResponse {
  device_code: string;
  user_code: string;
  verification_uri: string;
  expires_in_secs: number;
  interval_secs: number;
}

export interface DeviceCodePollResult {
  status: "pending" | "expired" | "approved";
  relay?: RelayConnectionStatus;
}

export type DaemonTarget =
  | { kind: "local" }
  | {
      kind: "remote";
      relayUrl: string;
      sessionToken: string;
      deviceId: string;
      deviceName: string;
    };

function relayWsUrl(relayUrl: string, path: string): string {
  const trimmed = relayUrl.trim().replace(/\/+$/, "");
  if (trimmed.startsWith("https://")) return `wss://${trimmed.slice("https://".length)}${path}`;
  if (trimmed.startsWith("http://")) return `ws://${trimmed.slice("http://".length)}${path}`;
  if (trimmed.startsWith("ws://") || trimmed.startsWith("wss://")) return `${trimmed}${path}`;
  return `ws://${trimmed}${path}`;
}

type EventListener = (rawEventsJson: string) => void;
type PluginListener = (rawPayload: string) => void;
type TerminalOutputListener = (terminalId: string, data: string) => void;
type TerminalExitListener = (terminalId: string) => void;
type CwdListener = (cwd: string) => void;

interface PendingRpc {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
}

const DAEMON_PROTOCOL_VERSION = 1;
const RECONNECT_BACKOFF_MS = [150, 300, 600, 1000, 1500];

let activeClient: DaemonAgent | null = null;

export function getActiveDaemonClient(): DaemonAgent | null {
  return activeClient;
}

function parseEndpointLine(text: string): DaemonEndpoint | null {
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line.startsWith("{")) continue;
    try {
      const parsed = JSON.parse(line) as Partial<DaemonEndpoint>;
      if (
        typeof parsed.address === "string" &&
        typeof parsed.token === "string" &&
        typeof parsed.protocol === "number"
      ) {
        return {
          protocol: parsed.protocol,
          address: parsed.address,
          token: parsed.token,
          pid: typeof parsed.pid === "number" ? parsed.pid : 0,
        };
      }
    } catch {
      // Ignore non-JSON lines on stdout.
    }
  }
  return null;
}

function snapshotToEvents(snapshot: Record<string, any>): Record<string, any>[] {
  const sessionId = snapshot.session_id;
  const events: Record<string, any>[] = [];
  if (snapshot.configuration) {
    const c = snapshot.configuration;
    events.push({
      SessionConfigured: {
        session_id: sessionId,
        cwd: c.cwd,
        model: c.model,
        provider_name: c.provider_name,
        provider_id: c.provider_id ?? null,
        model_id: c.model_id ?? null,
        reasoning_level: c.reasoning_level ?? null,
        permissions: c.permissions,
        plan_enabled: c.plan_enabled ?? false,
        bot_id: c.bot_id ?? null,
        global: Boolean(c.global),
      },
    });
  }
  events.push({
    ConversationSnapshot: {
      session_id: sessionId,
      agent_id: null,
      title: null,
      subtitle: null,
      items: snapshot.items ?? [],
      running: Boolean(snapshot.running),
      run_id: null,
    },
  });
  events.push({
    QueueChanged: {
      session_id: sessionId,
      items: snapshot.pending_queue ?? [],
    },
  });
  events.push({
    SteerChanged: {
      session_id: sessionId,
      items: snapshot.steer_queue ?? [],
    },
  });
  events.push({
    TodoListChanged: {
      session_id: sessionId,
      items: snapshot.todos ?? [],
    },
  });
  events.push({
    TaskListChanged: {
      session_id: sessionId,
      tasks: snapshot.tasks ?? [],
    },
  });
  events.push({
    TaskNotificationQueueChanged: {
      session_id: sessionId,
      notifications: snapshot.task_notifications ?? [],
    },
  });
  if (snapshot.context_usage) {
    const u = snapshot.context_usage;
    events.push({
      ContextUsage: {
        session_id: sessionId,
        used: u.used,
        total: u.total,
        cache_read_total: u.cache_read_total ?? 0,
        input_total: u.input_total ?? 0,
      },
    });
  }
  if (snapshot.context_breakdown) {
    events.push({
      ContextBreakdownChanged: {
        session_id: sessionId,
        breakdown: snapshot.context_breakdown,
      },
    });
  }
  for (const approval of snapshot.pending_approvals ?? []) {
    events.push({
      ApprovalRequested: {
        session_id: sessionId,
        request_id: approval.request_id,
        tool: approval.tool,
        detail: approval.detail,
        danger_key: approval.danger_key ?? null,
        requested_write_roots: approval.requested_write_roots ?? [],
        available_decisions: approval.available_decisions ?? [],
      },
    });
  }
  for (const question of snapshot.pending_questions ?? []) {
    events.push({
      QuestionRequested: {
        session_id: sessionId,
        request_id: question.request_id,
        questions: question.questions,
      },
    });
  }
  return events;
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function decodeText(frame: string | Uint8Array): string {
  if (typeof frame === "string") return frame;
  return new TextDecoder().decode(frame);
}

function processCwdOption(): { cwd?: string } {
  if (process.dataDir) return {};
  try {
    const home = os.homedir();
    if (home) return { cwd: home };
  } catch {
    // Ignore if homedir is unavailable.
  }
  return {};
}

export class DaemonAgent {
  private readonly selftest: boolean;
  private cwdValue = ".";
  private target: DaemonTarget = { kind: "local" };
  private targetGeneration = 0;
  private localEndpoint: DaemonEndpoint | null = null;
  private socket: WebSocketConnection | null = null;
  private child: ChildProcess | null = null;
  private cursor: EventCursor | null = null;
  private activeSessionId: string | null = null;
  private connectPromise: Promise<void> | null = null;
  private disposed = false;
  private rpcSeq = 0;
  private writeChain: Promise<void> = Promise.resolve();
  private readonly queuedFrames: string[] = [];
  private readonly pendingRpcs = new Map<string, PendingRpc>();
  private readonly eventListeners = new Set<EventListener>();
  private readonly pluginListeners = new Set<PluginListener>();
  private readonly terminalOutputListeners = new Set<TerminalOutputListener>();
  private readonly terminalExitListeners = new Set<TerminalExitListener>();
  private readonly cwdListeners = new Set<CwdListener>();
  private readonly bufferedEventBatches: string[] = [];
  private readonly bufferedPluginEvents: string[] = [];

  constructor(_cwd: string | null = null, selftest = false) {
    this.selftest = selftest;
    activeClient = this;
  }

  get cwd(): string {
    return this.cwdValue;
  }

  get currentTarget(): DaemonTarget {
    return this.target;
  }

  async switchTarget(next: DaemonTarget): Promise<void> {
    this.targetGeneration += 1;
    this.target = next;
    const oldSocket = this.socket;
    this.socket = null;
    this.connectPromise = null;
    this.cursor = null;
    this.activeSessionId = null;
    this.cwdValue = ".";
    this.queuedFrames.length = 0;
    for (const [, pending] of this.pendingRpcs) {
      pending.reject(new Error("switched target device"));
    }
    this.pendingRpcs.clear();
    if (oldSocket) {
      try {
        await oldSocket.close();
      } catch {
        // Ignore close errors while switching targets.
      }
    }
    await this.ensureConnected();
  }

  onCwdChange(listener: CwdListener): () => void {
    this.cwdListeners.add(listener);
    if (this.cwdValue && this.cwdValue !== ".") listener(this.cwdValue);
    return () => this.cwdListeners.delete(listener);
  }

  onTerminalOutput(listener: TerminalOutputListener): () => void {
    this.terminalOutputListeners.add(listener);
    return () => this.terminalOutputListeners.delete(listener);
  }

  onTerminalExit(listener: TerminalExitListener): () => void {
    this.terminalExitListeners.add(listener);
    return () => this.terminalExitListeners.delete(listener);
  }

  async listen(listener: EventListener): Promise<void> {
    this.eventListeners.add(listener);
    if (this.bufferedEventBatches.length > 0) {
      const pending = this.bufferedEventBatches.splice(0);
      for (const batch of pending) listener(batch);
    }
    await this.ensureConnected();
  }

  async listenPlugins(listener: PluginListener): Promise<void> {
    this.pluginListeners.add(listener);
    if (this.bufferedPluginEvents.length > 0) {
      const pending = this.bufferedPluginEvents.splice(0);
      for (const payload of pending) listener(payload);
    }
    await this.ensureConnected();
  }

  private ensureConnected(): Promise<void> {
    if (this.socket) return Promise.resolve();
    if (!this.connectPromise) {
      this.connectPromise = this.connectInternal().catch((error) => {
        this.connectPromise = null;
        this.emitBridgeError(error instanceof Error ? error.message : String(error));
        throw error;
      });
    }
    return this.connectPromise;
  }

  private async readEndpointFile(): Promise<DaemonEndpoint | null> {
    const candidates: string[] = [];
    const dataDir = process.env.PIG_DATA_DIR;
    if (dataDir) {
      candidates.push(`${dataDir.replace(/[\\/]+$/, "")}/daemon/endpoint.json`);
    }
    try {
      const home = os.homedir();
      if (home) {
        candidates.push(`${home.replace(/[\\/]+$/, "")}/.pigcode/daemon/endpoint.json`);
      }
    } catch {
      // Ignore if homedir is unavailable.
    }
    for (const filePath of candidates) {
      try {
        const raw = await fs.readFile(filePath, "utf8");
        const endpoint = parseEndpointLine(raw);
        if (endpoint && endpoint.protocol === DAEMON_PROTOCOL_VERSION) {
          return endpoint;
        }
      } catch {
        // Missing, unreadable, or outside fs.read grant: try next candidate.
      }
    }
    return null;
  }

  private async connectDirectOrSpawn(): Promise<WebSocketConnection> {
    const env = process.env;
    if (!this.selftest && env.PIG_DAEMON_URL && env.PIG_DAEMON_TOKEN) {
      return this.openSocket({
        protocol: DAEMON_PROTOCOL_VERSION,
        address: env.PIG_DAEMON_URL.replace(/^ws:\/\//, "").replace(/\/.*$/, ""),
        token: env.PIG_DAEMON_TOKEN,
        pid: 0,
      });
    }
    if (this.selftest) {
      const endpoint = await this.spawnAndReadEndpoint(["--selftest"]);
      return this.openSocket(endpoint);
    }
    // 1. Read ~/.pigcode/daemon/endpoint.json directly so Desktop can attach to
    // an externally started `cargo run -p pig-daemon` without requiring
    // `pig-daemon` to be on PATH.
    const fileEndpoint = await this.readEndpointFile();
    if (fileEndpoint) {
      try {
        return await this.openSocket(fileEndpoint);
      } catch {
        // Stale endpoint.json from a previous daemon: fall through to launch.
      }
    }
    // 2. Ask `pig-daemon --discover` (handles custom data dirs when on PATH).
    try {
      const discovered = await process.run("pig-daemon", ["--discover"], {
        ...processCwdOption(),
        timeoutMs: 5000,
      });
      if (discovered.code === 0) {
        const endpoint = parseEndpointLine(discovered.stdout);
        if (endpoint) return await this.openSocket(endpoint);
      }
    } catch {
      // Fall through to launch pig-daemon.
    }
    // 3. Launch pig-daemon automatically.
    try {
      if (process.platform !== "windows") {
        const out = await process.run("pig-daemon", ["--daemonize"], {
          ...processCwdOption(),
          timeoutMs: 15000,
        });
        if (out.code !== 0) {
          throw new Error(out.stderr.trim() || `pig-daemon --daemonize failed (${out.code ?? "signal"})`);
        }
        const endpoint = parseEndpointLine(out.stdout);
        if (!endpoint) throw new Error("pig-daemon --daemonize did not emit a valid endpoint");
        return await this.openSocket(endpoint);
      }
      // On Windows NSH places `process.run` children in a non-breakaway Job Object
      // that terminates descendants on exit, so launch the foreground daemon with
      // `process.spawn` when no daemon is already running.
      const endpoint = await this.spawnAndReadEndpoint(["--print-endpoint"]);
      return await this.openSocket(endpoint);
    } catch (error) {
      const code = (error as { code?: string } | null)?.code;
      const message = error instanceof Error ? error.message : String(error);
      if (code === "ENOENT" || message.includes("was not found in the host PATH")) {
        throw new Error(
          'pig-daemon is not running and was not found in PATH. Start it with "cargo run -p pig-daemon" or install it with "cargo install --path crates/pig-daemon".',
        );
      }
      throw error;
    }
  }

  private spawnAndReadEndpoint(args: string[]): Promise<DaemonEndpoint> {
    return new Promise<DaemonEndpoint>((resolve, reject) => {
      let settled = false;
      let stderr = "";
      let child: ChildProcess;
      try {
        child = process.spawn("pig-daemon", args, { ...processCwdOption(), lines: true });
      } catch (error) {
        reject(error instanceof Error ? error : new Error(String(error)));
        return;
      }
      this.child = child;
      const timer = setTimeout(() => {
        if (settled) return;
        settled = true;
        reject(new Error(`timed out waiting for pig-daemon ${args.join(" ")} endpoint`));
      }, 15000);
      child.on("stdout", (line) => {
        if (settled) return;
        const endpoint = parseEndpointLine(line);
        if (endpoint) {
          settled = true;
          clearTimeout(timer);
          resolve(endpoint);
        }
      });
      child.on("stderr", (line) => {
        stderr += (stderr ? "\n" : "") + line;
      });
      child.on("exit", (result) => {
        if (this.child === child) this.child = null;
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        reject(
          new Error(
            stderr.trim() ||
              result.error ||
              `pig-daemon exited before publishing endpoint (code ${result.code ?? "null"})`,
          ),
        );
      });
    });
  }

  private async openSocket(endpoint: DaemonEndpoint): Promise<WebSocketConnection> {
    if (endpoint.protocol !== DAEMON_PROTOCOL_VERSION) {
      throw new Error(
        `unsupported daemon protocol ${endpoint.protocol} (expected ${DAEMON_PROTOCOL_VERSION})`,
      );
    }
    this.localEndpoint = endpoint;
    const conn = await websocket.connect(`ws://${endpoint.address}/`, {
      headers: { Authorization: `Bearer ${endpoint.token}` },
    });
    const subscribeFrame = JSON.stringify({
      kind: "subscribe",
      after: this.cursor,
    });
    await conn.write(subscribeFrame);
    return conn;
  }

  private async openRemoteSocket(target: Extract<DaemonTarget, { kind: "remote" }>): Promise<WebSocketConnection> {
    const url = relayWsUrl(target.relayUrl, `/ws/client/${encodeURIComponent(target.deviceId)}`);
    const conn = await websocket.connect(url, {
      headers: { Authorization: `Bearer ${target.sessionToken}` },
    });
    const subscribeFrame = JSON.stringify({
      kind: "subscribe",
      after: this.cursor,
    });
    await conn.write(subscribeFrame);
    return conn;
  }

  private async openActiveTargetSocket(): Promise<WebSocketConnection> {
    if (this.target.kind === "remote") {
      return this.openRemoteSocket(this.target);
    }
    return this.connectDirectOrSpawn();
  }

  private async connectInternal(): Promise<void> {
    const generation = this.targetGeneration;
    const conn = await this.openActiveTargetSocket();
    if (this.disposed || generation !== this.targetGeneration) {
      try {
        await conn.close();
      } catch {
        // Ignore stale socket close errors.
      }
      return;
    }
    this.socket = conn;
    this.startReadLoop(conn);
    await this.sendBootstrap(false);
    await this.flushQueuedFrames();
  }

  private async sendBootstrap(resync: boolean): Promise<void> {
    const ops: Record<string, unknown>[] = [
      { ListSessions: null },
      { ListWorkspaces: null },
      { ListBots: null },
      { GetConfig: null },
    ];
    if (resync && this.activeSessionId) {
      ops.push({ OpenSession: { session_id: this.activeSessionId } });
    }
    for (const op of ops) {
      this.enqueueFrame(JSON.stringify({ kind: "op", op }));
    }
    void this.rpc<DaemonInfo>("daemon.info")
      .then((info) => {
        if (info && typeof info.cwd === "string" && info.cwd) {
          this.cwdValue = info.cwd;
          for (const listener of this.cwdListeners) listener(info.cwd);
        }
      })
      .catch(() => {
        // Ignore if daemon disconnected before info replied.
      });
  }

  private enqueueFrame(frame: string): void {
    const socket = this.socket;
    if (!socket) {
      this.queuedFrames.push(frame);
      void this.ensureConnected().catch(() => {});
      return;
    }
    this.writeChain = this.writeChain
      .then(() => socket.write(frame))
      .catch(() => {
        // Disconnect will be observed and handled by the read loop.
      });
  }

  private async flushQueuedFrames(): Promise<void> {
    const socket = this.socket;
    if (!socket || this.queuedFrames.length === 0) return;
    const frames = this.queuedFrames.splice(0);
    for (const frame of frames) {
      this.writeChain = this.writeChain.then(() => socket.write(frame)).catch(() => {});
    }
    await this.writeChain;
  }

  private startReadLoop(conn: WebSocketConnection): void {
    void (async () => {
      try {
        while (!this.disposed) {
          const frame = await conn.read();
          if (frame === null) break;
          this.handleMessage(decodeText(frame));
        }
      } catch (error) {
        if (!this.disposed) {
          this.handleDisconnect(conn, error instanceof Error ? error.message : String(error));
          return;
        }
      }
      if (!this.disposed) {
        this.handleDisconnect(conn, "daemon connection closed");
      }
    })();
  }

  private handleMessage(raw: string): void {
    let message: Record<string, any>;
    try {
      message = JSON.parse(raw) as Record<string, any>;
    } catch {
      return;
    }
    switch (message.kind) {
      case "subscription": {
        if (message.cursor) this.cursor = message.cursor as EventCursor;
        if (message.resync_required) {
          void this.sendBootstrap(true);
        }
        break;
      }
      case "batch": {
        if (typeof message.epoch === "string" && typeof message.sequence === "number") {
          this.cursor = { epoch: message.epoch, sequence: message.sequence };
        }
        const events = Array.isArray(message.events) ? message.events : [];
        if (events.length > 0) this.deliverEvents(JSON.stringify(events));
        break;
      }
      case "snapshot": {
        if (message.cursor) this.cursor = message.cursor as EventCursor;
        const events = snapshotToEvents(message);
        if (events.length > 0) this.deliverEvents(JSON.stringify(events));
        break;
      }
      case "rpc_result": {
        const pending = this.pendingRpcs.get(message.id);
        if (pending) {
          this.pendingRpcs.delete(message.id);
          if (typeof message.error === "string" && message.error.length > 0) {
            pending.reject(new Error(message.error));
          } else {
            pending.resolve(message.result);
          }
        }
        break;
      }
      case "plugin_event": {
        if (typeof message.payload === "string") {
          this.deliverPluginEvent(message.payload);
        }
        break;
      }
      case "terminal_output": {
        if (typeof message.terminal_id === "string" && typeof message.data === "string") {
          for (const listener of this.terminalOutputListeners) {
            listener(message.terminal_id, message.data);
          }
        }
        break;
      }
      case "terminal_exit": {
        if (typeof message.terminal_id === "string") {
          for (const listener of this.terminalExitListeners) {
            listener(message.terminal_id);
          }
        }
        break;
      }
      case "error": {
        if (typeof message.detail === "string") {
          this.emitBridgeError(message.detail);
        }
        break;
      }
    }
  }

  private handleDisconnect(conn: WebSocketConnection, reason: string): void {
    if (this.socket !== conn) return;
    this.socket = null;
    this.connectPromise = null;
    for (const [, pending] of this.pendingRpcs) {
      pending.reject(new Error(reason));
    }
    this.pendingRpcs.clear();
    if (this.disposed || this.selftest) {
      if (!this.disposed && reason !== "daemon connection closed") {
        this.emitBridgeError(reason);
      }
      return;
    }
    void this.reconnectLoop(reason);
  }

  private async reconnectLoop(initialReason: string): Promise<void> {
    const generation = this.targetGeneration;
    let lastError = initialReason;
    for (const ms of RECONNECT_BACKOFF_MS) {
      if (this.disposed || generation !== this.targetGeneration) return;
      await delay(ms);
      if (this.disposed || generation !== this.targetGeneration) return;
      try {
        const conn = await this.openActiveTargetSocket();
        if (this.disposed || generation !== this.targetGeneration) {
          try {
            await conn.close();
          } catch {
            // Ignore stale socket close errors.
          }
          return;
        }
        this.socket = conn;
        this.startReadLoop(conn);
        await this.flushQueuedFrames();
        return;
      } catch (error) {
        lastError = error instanceof Error ? error.message : String(error);
      }
    }
    if (generation === this.targetGeneration) {
      this.emitBridgeError(lastError);
    }
  }

  private deliverEvents(encoded: string): void {
    if (this.eventListeners.size === 0) {
      this.bufferedEventBatches.push(encoded);
      return;
    }
    for (const listener of this.eventListeners) listener(encoded);
  }

  private deliverPluginEvent(payload: string): void {
    if (this.pluginListeners.size === 0) {
      this.bufferedPluginEvents.push(payload);
      return;
    }
    for (const listener of this.pluginListeners) listener(payload);
  }

  private emitBridgeError(detail: string): void {
    this.deliverEvents(JSON.stringify([{ BridgeError: { detail } }]));
  }

  async rpc<T = unknown>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    await this.ensureConnected();
    const id = `rpc-${++this.rpcSeq}`;
    return new Promise<T>((resolve, reject) => {
      this.pendingRpcs.set(id, {
        resolve: resolve as (value: unknown) => void,
        reject,
      });
      this.enqueueFrame(JSON.stringify({ kind: "rpc", id, method, params }));
    });
  }

  /**
   * Execute an RPC against the local pig-daemon even when the active session
   * target is switched to a remote device over pig-relay.
   */
  async localRpc<T = unknown>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    if (this.target.kind === "local") {
      return this.rpc<T>(method, params);
    }
    const endpoint = this.localEndpoint ?? (await this.readEndpointFile());
    if (!endpoint) {
      throw new Error("local pig-daemon endpoint is unavailable");
    }
    const conn = await websocket.connect(`ws://${endpoint.address}/`, {
      headers: { Authorization: `Bearer ${endpoint.token}` },
    });
    const id = `local-rpc-${++this.rpcSeq}`;
    try {
      await conn.write(JSON.stringify({ kind: "subscribe", after: null }));
      await conn.write(JSON.stringify({ kind: "rpc", id, method, params }));
      while (true) {
        const frame = await conn.read();
        if (frame === null) {
          throw new Error("local pig-daemon closed before RPC replied");
        }
        const message = JSON.parse(decodeText(frame)) as Record<string, any>;
        if (message.kind === "rpc_result" && message.id === id) {
          if (typeof message.error === "string" && message.error.length > 0) {
            throw new Error(message.error);
          }
          return message.result as T;
        }
      }
    } finally {
      try {
        await conn.close();
      } catch {
        // Ignore close errors.
      }
    }
  }

  relayStatus(): Promise<RelayConnectionStatus> {
    return this.localRpc<RelayConnectionStatus>("relay.status");
  }

  relayRegister(
    relayUrl: string,
    username: string,
    password: string,
    deviceName?: string,
  ): Promise<RelayConnectionStatus> {
    return this.localRpc<RelayConnectionStatus>("relay.register", {
      relay_url: relayUrl,
      username,
      password,
      ...(deviceName ? { device_name: deviceName } : {}),
    });
  }

  relayLogin(
    relayUrl: string,
    username: string,
    password: string,
    deviceName?: string,
  ): Promise<RelayConnectionStatus> {
    return this.localRpc<RelayConnectionStatus>("relay.login", {
      relay_url: relayUrl,
      username,
      password,
      ...(deviceName ? { device_name: deviceName } : {}),
    });
  }

  relayDeviceStart(relayUrl: string, deviceName?: string): Promise<DeviceCodeStartResponse> {
    return this.localRpc<DeviceCodeStartResponse>("relay.device_start", {
      relay_url: relayUrl,
      ...(deviceName ? { device_name: deviceName } : {}),
    });
  }

  relayDevicePoll(
    relayUrl: string,
    deviceCode: string,
    deviceName?: string,
  ): Promise<DeviceCodePollResult> {
    return this.localRpc<DeviceCodePollResult>("relay.device_poll", {
      relay_url: relayUrl,
      device_code: deviceCode,
      ...(deviceName ? { device_name: deviceName } : {}),
    });
  }

  relayApproveCode(userCode: string): Promise<boolean> {
    return this.localRpc<boolean>("relay.approve_code", { user_code: userCode });
  }

  relayLogout(): Promise<RelayConnectionStatus> {
    return this.localRpc<RelayConnectionStatus>("relay.logout");
  }

  relayDevices(): Promise<RelayDeviceInfo[]> {
    return this.localRpc<RelayDeviceInfo[]>("relay.devices");
  }

  relayRevokeDevice(deviceId: string): Promise<boolean> {
    return this.localRpc<boolean>("relay.revoke_device", { device_id: deviceId });
  }

  relaySyncBots(): Promise<Record<string, unknown>> {
    return this.localRpc<Record<string, unknown>>("relay.sync_bots");
  }

  listBots(): Promise<unknown[]> {
    return this.rpc<unknown[]>("bot.list");
  }

  upsertBot(bot: Record<string, unknown>): Promise<boolean> {
    return this.rpc<boolean>("bot.upsert", { bot });
  }

  deleteBot(botId: string): Promise<boolean> {
    return this.rpc<boolean>("bot.delete", { bot_id: botId });
  }

  send(opJson: string): void {
    const op = JSON.parse(opJson) as Record<string, unknown>;
    if (op && typeof op === "object" && "OpenSession" in op) {
      const payload = op.OpenSession as { session_id?: string } | null;
      if (payload?.session_id) this.activeSessionId = payload.session_id;
    }
    this.enqueueFrame(JSON.stringify({ kind: "op", op }));
  }

  openSession(sessionId: string): void {
    this.activeSessionId = sessionId;
    this.enqueueFrame(JSON.stringify({ kind: "op", op: { OpenSession: { session_id: sessionId } } }));
  }

  listFonts(): Promise<string[]> {
    return this.rpc<string[]>("system.list_fonts");
  }

  sandboxStatus(): Promise<Record<string, unknown>> {
    return this.rpc<Record<string, unknown>>("system.sandbox_status");
  }

  processSample(): Promise<Record<string, unknown>> {
    return this.rpc<Record<string, unknown>>("system.process_sample");
  }

  highlight(code: string, language: string): Promise<Array<{ start: number; end: number; token: string }>> {
    return this.rpc("code.highlight", { code, language });
  }

  searchSessionContent(query: string): Promise<unknown[]> {
    return this.rpc<unknown[]>("session.search_content", { query });
  }

  readFile(path: string, cwd: string): Promise<{ content: string; truncated: boolean }> {
    return this.rpc("fs.read_file", { path, cwd });
  }

  listDirectory(path?: string | null, includeFiles = false): Promise<DirectoryListing> {
    return this.rpc("fs.list_directory", {
      ...(path ? { path } : {}),
      include_files: includeFiles,
    });
  }

  sessionImageSource(sessionId: string, imageNumber: number): Promise<string> {
    return this.rpc<string>("session.image_source", {
      session_id: sessionId,
      image_number: imageNumber,
    });
  }

  sessionImage(sessionId: string, imageNumber: number): Promise<{ bytes: number[]; mime: string }> {
    return this.rpc("session.image", {
      session_id: sessionId,
      image_number: imageNumber,
    });
  }

  openTerminal(cwd: string, shell: string | null): Promise<TerminalInfo> {
    return this.rpc<TerminalInfo>("terminal.open", { cwd, ...(shell ? { shell } : {}) });
  }

  writeTerminal(terminalId: string, data: string): Promise<boolean> {
    return this.rpc<boolean>("terminal.write", { terminal_id: terminalId, data });
  }

  resizeTerminal(terminalId: string, cols: number, rows: number): Promise<boolean> {
    return this.rpc<boolean>("terminal.resize", { terminal_id: terminalId, cols, rows });
  }

  closeTerminal(terminalId: string): Promise<boolean> {
    return this.rpc<boolean>("terminal.close", { terminal_id: terminalId });
  }

  syncPlugins(): Promise<boolean> {
    return this.rpc<boolean>("plugin.sync");
  }

  setPluginEnabled(pkg: string, digest: string, enabled: boolean): Promise<boolean> {
    return this.rpc<boolean>("plugin.set_enabled", { package: pkg, digest, enabled });
  }

  installPlugin(packagePath: string, rootPath: string): Promise<unknown> {
    return this.rpc("plugin.install", { package: packagePath, root: rootPath });
  }

  uninstallPlugin(pkg: string, digest: string): Promise<boolean> {
    return this.rpc<boolean>("plugin.uninstall", { package: pkg, digest });
  }

  retryPlugin(pkg: string, digest: string): Promise<boolean> {
    return this.rpc<boolean>("plugin.retry", { package: pkg, digest });
  }

  setPluginLocale(locale: string): Promise<boolean> {
    return this.rpc<boolean>("plugin.set_locale", { locale });
  }

  dispatchPluginCommand(instance: string, command: string): Promise<boolean> {
    return this.rpc<boolean>("plugin.dispatch_command", { instance, command });
  }

  loadThemes(packagesJson: string): Promise<{ themes: unknown[]; errors: string[] }> {
    return this.rpc("plugin.load_themes", { packages: packagesJson });
  }

  marketplaceCatalog(indexUrl?: string | null): Promise<unknown> {
    return this.rpc("plugin.marketplace_catalog", indexUrl ? { index_url: indexUrl } : {});
  }

  marketplaceSources(): Promise<unknown[]> {
    return this.rpc<unknown[]>("plugin.marketplace_sources");
  }

  previewMarketplace(indexUrl: string): Promise<unknown> {
    return this.rpc("plugin.preview_marketplace", { index_url: indexUrl });
  }

  addMarketplace(previewJson: string): Promise<unknown[]> {
    return this.rpc<unknown[]>("plugin.add_marketplace", { preview: previewJson });
  }

  removeMarketplace(id: string): Promise<unknown[]> {
    return this.rpc<unknown[]>("plugin.remove_marketplace", { id });
  }

  installMarketplacePlugin(
    packageJson: string,
    indexUrl?: string | null,
    root?: string | null,
  ): Promise<unknown> {
    return this.rpc("plugin.install_marketplace", {
      package: packageJson,
      ...(indexUrl ? { index_url: indexUrl } : {}),
      ...(root ? { root } : {}),
    });
  }

  dispose(): void {
    this.disposed = true;
    const socket = this.socket;
    this.socket = null;
    if (socket) {
      void socket.close().catch(() => {});
    }
    const child = this.child;
    this.child = null;
    if (child) {
      try {
        child.kill();
      } catch {
        // Ignore teardown races.
      }
    }
    if (activeClient === this) activeClient = null;
  }
}
