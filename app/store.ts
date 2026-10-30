import { signal } from "nsh";
import { Agent } from "pig-code-ts/native";
import {
  AppConfig,
  ChatItem,
  MarkdownItem,
  NoteItem,
  PendingApproval,
  PluginPackageInfo,
  SessionMeta,
  ToolItem,
  errorMessage,
  eventKind,
} from "./protocol";
import { PluginThemeDefinition } from "./themes";

let generatedId = 0;

export class AgentStore {
  readonly agent: Agent;
  readonly cwd;
  readonly draft = signal("");
  readonly sessions = signal<SessionMeta[]>([]);
  readonly current = signal<string | null>(null);
  readonly currentTitle = signal("新任务");
  readonly currentModel = signal("");
  readonly items = signal<Record<string, ChatItem[]>>({});
  readonly running = signal<Record<string, boolean>>({});
  readonly approvals = signal<Record<string, PendingApproval[]>>({});
  readonly errors = signal<Record<string, string>>({});
  readonly contextUsage = signal<Record<string, string>>({});
  readonly outputRate = signal<Record<string, number>>({});
  readonly config = signal<AppConfig | null>(null);
  readonly globalError = signal("");
  readonly pluginPackages = signal<PluginPackageInfo[]>([]);
  readonly pluginThemes = signal<PluginThemeDefinition[]>([]);
  readonly themeError = signal("");
  private autoOpen = true;
  private pendingText: string | null = null;
  private themeLoadId = 0;
  private rateSession: string | null = null;
  private rateStartedAt = 0;
  private rateCharacters = 0;

  constructor(selftest = false) {
    this.agent = new Agent(null, selftest);
    this.cwd = signal(this.agent.cwd);
  }

  start(): void {
    void this.agent.listen((raw) => {
      try {
        const events = JSON.parse(raw);
        for (const event of events) this.reduce(event);
      } catch (error) {
        this.globalError.set(`事件解析失败：${errorMessage(error)}`);
      }
    });
  }

  newTask(): void {
    this.autoOpen = false;
    this.pendingText = null;
    this.current.set(null);
    this.currentTitle.set("新任务");
    this.currentModel.set("");
    this.globalError.set("");
  }

  openSession(sessionId: string): void {
    this.autoOpen = false;
    this.pendingText = null;
    try {
      this.agent.openSession(sessionId);
    } catch (error) {
      this.globalError.set(errorMessage(error));
    }
  }

  send(): void {
    const text = this.draft().trim();
    if (!text) return;
    const sessionId = this.current();
    try {
      if (sessionId && this.running()[sessionId]) {
        this.agent.interrupt(sessionId);
        return;
      }
      if (sessionId) {
        this.agent.sendMessage(sessionId, text);
      } else {
        this.pendingText = text;
        this.agent.newSession(this.cwd());
      }
      this.draft.set("");
      this.globalError.set("");
    } catch (error) {
      this.pendingText = null;
      this.globalError.set(errorMessage(error));
    }
  }

  answerApproval(allow: boolean): void {
    const sessionId = this.current();
    if (!sessionId) return;
    const approval = (this.approvals()[sessionId] ?? [])[0];
    if (!approval) return;
    try {
      this.agent.answerApproval(approval.requestId, allow);
      this.approvals.update((all) => ({
        ...all,
        [sessionId]: (all[sessionId] ?? []).filter(
          (item) => item.requestId !== approval.requestId,
        ),
      }));
    } catch (error) {
      this.setError(sessionId, errorMessage(error));
    }
  }

  reportLayout(width: number, height: number): void {
    try {
      this.agent.reportLayout(width, height);
    } catch (error) {
      this.globalError.set(errorMessage(error));
    }
  }

  saveThemePreference(theme: string, mode: "system" | "light" | "dark"): void {
    const config = this.config();
    if (!config) return;
    try {
      this.agent.send(
        JSON.stringify({
          SaveConfig: { config: { ...config, theme, theme_mode: mode } },
        }),
      );
    } catch (error) {
      this.globalError.set(errorMessage(error));
    }
  }

  syncPlugins(): void {
    try {
      this.agent.syncPlugins();
    } catch (error) {
      this.globalError.set(errorMessage(error));
    }
  }

  setPluginEnabled(plugin: PluginPackageInfo, enabled: boolean): void {
    try {
      this.agent.setPluginEnabled(plugin.id, plugin.digest, enabled);
    } catch (error) {
      this.globalError.set(errorMessage(error));
    }
  }

  private async loadPluginThemes(packages: PluginPackageInfo[]): Promise<void> {
    const loadId = ++this.themeLoadId;
    try {
      const raw = await this.agent.loadThemes(JSON.stringify(packages));
      const result = JSON.parse(raw) as {
        themes: PluginThemeDefinition[];
        errors: string[];
      };
      if (loadId !== this.themeLoadId) return;
      this.pluginThemes.set(result.themes);
      this.themeError.set(result.errors.join("\n"));
    } catch (error) {
      if (loadId === this.themeLoadId) this.themeError.set(errorMessage(error));
    }
  }

  private reduce(event: Record<string, any>): void {
    const kind = eventKind(event);
    const data = event[kind] ?? {};
    const sessionId = typeof data.session_id === "string" ? data.session_id : null;
    switch (kind) {
      case "SessionList": {
        const sessions = (data.sessions as SessionMeta[]).filter((item) => !item.archived);
        this.sessions.set(sessions);
        if (this.autoOpen && !this.current() && sessions.length > 0) {
          this.autoOpen = false;
          this.agent.openSession(sessions[0].id);
        }
        break;
      }
      case "SessionConfigured": {
        this.autoOpen = false;
        this.current.set(sessionId);
        this.cwd.set(data.cwd);
        this.currentModel.set(data.model || "");
        this.currentTitle.set(
          this.sessions().find((item) => item.id === sessionId)?.title ?? "当前会话",
        );
        const pending = this.pendingText;
        this.pendingText = null;
        if (pending) this.agent.sendMessage(sessionId!, pending);
        break;
      }
      case "SessionTitleChanged":
        this.sessions.update((sessions) =>
          sessions.map((item) =>
            item.id === sessionId ? { ...item, title: data.title } : item,
          ),
        );
        if (this.current() === sessionId) this.currentTitle.set(data.title);
        break;
      case "PluginSnapshot": {
        const packages = data.snapshot.packages as PluginPackageInfo[];
        this.pluginPackages.set(packages);
        void this.loadPluginThemes(packages);
        break;
      }
      case "ConfigSnapshot":
        this.config.set(data.config as AppConfig);
        break;
      case "UserMessage":
        this.updateItems(sessionId!, (items) => [
          ...items,
          { kind: "user", id: `user-${++generatedId}`, text: data.text },
        ]);
        break;
      case "ReasoningDelta":
        this.recordRateDelta(sessionId!, String(data.delta ?? ""));
        this.upsertMarkdown(sessionId!, data.item_id, "reasoning", data.delta, true, true);
        break;
      case "TextDelta":
        this.recordRateDelta(sessionId!, String(data.delta ?? ""));
        this.upsertMarkdown(sessionId!, data.item_id, "markdown", data.delta, true, true);
        break;
      case "TextDone":
        this.upsertMarkdown(
          sessionId!,
          data.item_id,
          "markdown",
          data.full_text,
          false,
          false,
        );
        break;
      case "ToolInputStart":
        this.upsertTool(sessionId!, data.item_id, {
          tool: data.tool,
          status: "input",
        });
        break;
      case "ToolInputDelta":
        this.upsertTool(sessionId!, data.item_id, { detail: data.delta }, true);
        break;
      case "ToolInputDiscard":
        this.updateItems(sessionId!, (items) =>
          items.filter((item) => item.id !== data.item_id),
        );
        break;
      case "ToolCallBegin":
        this.upsertTool(sessionId!, data.item_id, {
          tool: data.tool,
          summary: data.input_summary,
          detail: data.detail,
          status: "running",
        });
        break;
      case "ToolCallEnd":
        this.upsertTool(sessionId!, data.item_id, {
          output: data.output,
          status: data.stopped ? "stopped" : data.is_error ? "error" : "done",
          edit: data.edit,
        });
        break;
      case "ApprovalRequested":
        this.approvals.update((all) => {
          const queue = all[sessionId!] ?? [];
          if (queue.some((item) => item.requestId === data.request_id)) return all;
          return {
            ...all,
            [sessionId!]: [
              ...queue,
              {
                requestId: data.request_id,
                tool: data.tool,
                detail: data.detail,
              },
            ],
          };
        });
        break;
      case "TurnStarted":
        this.startRate(sessionId!);
        this.setRunning(sessionId!, true);
        break;
      case "TurnComplete":
      case "TurnAborted":
        this.setRunning(sessionId!, false);
        this.approvals.update((all) => ({ ...all, [sessionId!]: [] }));
        break;
      case "ContextUsage":
        this.contextUsage.update((all) => ({
          ...all,
          [sessionId!]: `${data.used.toLocaleString()} / ${data.total.toLocaleString()} tokens`,
        }));
        break;
      case "CompactStarted":
        this.addNote(sessionId!, "正在压缩上下文…", false);
        break;
      case "ContextCompacted":
        this.addNote(sessionId!, data.note, false);
        break;
      case "Error":
        if (sessionId) {
          this.setError(sessionId, errorMessage(data.error));
          if (data.fatal !== false) this.setRunning(sessionId, false);
        } else {
          this.globalError.set(errorMessage(data.error));
        }
        break;
      case "BridgeError":
        this.globalError.set(data.detail);
        break;
    }
  }

  private upsertMarkdown(
    sessionId: string,
    id: string,
    kind: "markdown" | "reasoning",
    text: string,
    append: boolean,
    streaming: boolean,
  ): void {
    this.updateItems(sessionId, (items) => {
      const index = items.findIndex((item) => item.id === id);
      if (index < 0) return [...items, { kind, id, text, streaming }];
      const next = [...items];
      const previous = next[index] as MarkdownItem;
      next[index] = {
        ...previous,
        text: append ? previous.text + text : text,
        streaming,
      };
      return next;
    });
  }

  private upsertTool(
    sessionId: string,
    id: string,
    changes: Partial<Omit<ToolItem, "kind" | "id">>,
    appendDetail = false,
  ): void {
    this.updateItems(sessionId, (items) => {
      const index = items.findIndex((item) => item.id === id);
      if (index < 0) {
        return [
          ...items,
          {
            kind: "tool",
            id,
            tool: changes.tool ?? "Tool",
            summary: changes.summary ?? "",
            detail: changes.detail ?? "",
            output: changes.output ?? "",
            status: changes.status ?? "running",
            edit: changes.edit,
          } satisfies ToolItem,
        ];
      }
      const next = [...items];
      const previous = next[index] as ToolItem;
      next[index] = {
        ...previous,
        ...changes,
        detail: appendDetail ? previous.detail + (changes.detail ?? "") : (changes.detail ?? previous.detail),
      };
      return next;
    });
  }

  private startRate(sessionId: string): void {
    this.rateSession = sessionId;
    this.rateStartedAt = Date.now();
    this.rateCharacters = 0;
    this.outputRate.update((all) => ({ ...all, [sessionId]: 0 }));
  }

  private recordRateDelta(sessionId: string, delta: string): void {
    if (this.rateSession !== sessionId) this.startRate(sessionId);
    this.rateCharacters += delta.length;
    const elapsed = Math.max((Date.now() - this.rateStartedAt) / 1000, 0.05);
    const tokensPerSecond = Math.max(0, Math.round(this.rateCharacters / 4 / elapsed));
    this.outputRate.update((all) => ({ ...all, [sessionId]: tokensPerSecond }));
  }

  private addNote(sessionId: string, text: string, error: boolean): void {
    this.updateItems(sessionId, (items) => [
      ...items,
      { kind: "note", id: `note-${++generatedId}`, text, error } satisfies NoteItem,
    ]);
  }

  private updateItems(
    sessionId: string,
    update: (items: ChatItem[]) => ChatItem[],
  ): void {
    this.items.update((all) => ({
      ...all,
      [sessionId]: update(all[sessionId] ?? []),
    }));
  }

  private setRunning(sessionId: string, value: boolean): void {
    this.running.update((all) => ({ ...all, [sessionId]: value }));
  }

  private setError(sessionId: string, message: string): void {
    this.errors.update((all) => ({ ...all, [sessionId]: message }));
  }
}
