import { signal } from "nsh";
import { Agent } from "pig-code-ts/native";
import {
  AppConfig,
  ApprovalDecision,
  ChatItem,
  ConnectionTestResult,
  HeldDisposition,
  MarkdownItem,
  MessageDelivery,
  NoteItem,
  Payload,
  PendingApproval,
  PendingQuestion,
  PermissionSettings,
  PluginPackageInfo,
  ProviderConfig,
  ProviderCredentialStatus,
  QueueItem,
  QueueState,
  SessionConfiguration,
  SessionMeta,
  ToolItem,
  errorMessage,
  eventKind,
} from "./protocol";
import { oppositeDelivery, parseSlashCommand, permissionsForMode } from "./commands";
import { PluginThemeDefinition } from "./themes";

let generatedId = 0;

const EMPTY_QUEUE: QueueState = { pending: [], steer: [], paused: null, held: null };

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
  readonly questions = signal<Record<string, PendingQuestion | null>>({});
  readonly queues = signal<Record<string, QueueState>>({});
  readonly configurations = signal<Record<string, SessionConfiguration>>({});
  readonly heroConfiguration = signal<SessionConfiguration>({
    providerId: null,
    modelId: null,
    reasoningLevel: null,
    permissions: permissionsForMode("auto"),
    planEnabled: false,
  });
  readonly errors = signal<Record<string, string>>({});
  readonly contextUsage = signal<Record<string, string>>({});
  readonly outputRate = signal<Record<string, number>>({});
  readonly config = signal<AppConfig | null>(null);
  readonly providerCredentials = signal<Record<string, ProviderCredentialStatus>>({});
  readonly testResults = signal<Record<string, ConnectionTestResult>>({});
  readonly testingProviders = signal<Record<string, boolean>>({});
  readonly globalError = signal("");
  readonly pluginPackages = signal<PluginPackageInfo[]>([]);
  readonly pluginThemes = signal<PluginThemeDefinition[]>([]);
  readonly themeError = signal("");
  private autoOpen = true;
  private pendingText: string | null = null;
  private pendingDelivery: MessageDelivery = "queue";
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
    this.syncCurrentModelName();
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

  configuration(): SessionConfiguration {
    const sessionId = this.current();
    return (sessionId && this.configurations()[sessionId]) || this.heroConfiguration();
  }

  currentQueue(): QueueState {
    const sessionId = this.current();
    return (sessionId && this.queues()[sessionId]) || EMPTY_QUEUE;
  }

  modelOptions() {
    return (this.config()?.providers ?? []).flatMap((provider) =>
      provider.enabled
        ? provider.models.filter((model) => model.enabled).map((model) => ({ provider, model }))
        : [],
    );
  }

  selectedModel() {
    const configuration = this.configuration();
    const providerId = configuration.providerId ?? this.config()?.default_provider;
    const modelId = configuration.modelId ?? this.config()?.default_model;
    return this.modelOptions().find(
      ({ provider, model }) => provider.id === providerId && model.id === modelId,
    );
  }

  reasoningOptions(): { id: string; label: string }[] {
    const model = this.selectedModel()?.model;
    return (model?.reasoning_levels ?? []).map((id) => ({
      id,
      label: model?.reasoning_labels?.[id] ?? id,
    }));
  }

  selectModel(providerId: string, modelId: string): void {
    const selected = this.modelOptions().find(
      ({ provider, model }) => provider.id === providerId && model.id === modelId,
    );
    if (!selected) return;
    const levels = selected.model.reasoning_levels ?? [];
    const previous = this.configuration().reasoningLevel;
    const reasoningLevel = selected.model.default_reasoning_level ??
      (previous && levels.includes(previous)
        ? previous
        : previous
          ? (levels.find((level) => !["none", "off", "disabled"].includes(level)) ?? null)
          : null);
    this.updateConfiguration({ providerId, modelId, reasoningLevel });
    this.syncCurrentModelName();
    const sessionId = this.current();
    if (sessionId) {
      this.command({ SetModel: { session_id: sessionId, provider_id: providerId, model_id: modelId, reasoning_level: reasoningLevel } });
    }
  }

  setReasoning(reasoningLevel: string | null): void {
    this.updateConfiguration({ reasoningLevel });
    const sessionId = this.current();
    if (sessionId) this.command({ SetReasoning: { session_id: sessionId, reasoning_level: reasoningLevel } });
  }

  setPermissions(permissions: PermissionSettings): void {
    this.updateConfiguration({ permissions });
    const sessionId = this.current();
    if (sessionId) this.command({ SetPermissions: { session_id: sessionId, permissions } });
  }

  setPlanMode(planEnabled: boolean): void {
    this.updateConfiguration({ planEnabled });
    const sessionId = this.current();
    if (sessionId) this.command({ SetPlanMode: { session_id: sessionId, enabled: planEnabled } });
  }

  send(delivery?: MessageDelivery): void {
    let text = this.draft().trim();
    if (!text) return;
    const sessionId = this.current();
    if (sessionId && (this.questions()[sessionId] || (this.approvals()[sessionId] ?? []).length > 0)) {
      return;
    }
    const running = sessionId ? (this.running()[sessionId] ?? false) : false;
    const slash = parseSlashCommand(text);
    if (slash?.name === "/plan") {
      const instruction = slash.instruction;
      if (instruction?.toLowerCase() === "on" || instruction?.toLowerCase() === "off") {
        this.setPlanMode(instruction.toLowerCase() === "on");
        this.draft.set("");
        return;
      }
      this.setPlanMode(instruction ? true : !this.configuration().planEnabled);
      if (!instruction) {
        this.draft.set("");
        return;
      }
      text = instruction;
    } else if (slash) {
      if (running) {
        this.globalError.set("请先停止当前回合，再执行这个命令。");
        return;
      }
      if (slash.name === "/clear") {
        this.draft.set("");
        this.clearTranscript();
        return;
      }
      if (!sessionId) {
        this.globalError.set("请先发送一条消息创建会话。");
        return;
      }
      if (this.command({ Compact: { session_id: sessionId, instruction: slash.instruction ?? null } })) {
        this.draft.set("");
      }
      return;
    }
    const configuration = this.configuration();
    const requestedDelivery = delivery ?? this.config()?.default_delivery ?? "queue";
    if (sessionId) {
      if (this.command({
        SendMessage: {
          session_id: sessionId,
          content: text,
          files: [],
          images: [],
          permissions: configuration.permissions,
          delivery: running ? requestedDelivery : "queue",
        },
      })) {
        this.draft.set("");
      }
      return;
    }
    if (this.command({
      NewSession: {
        cwd: this.cwd(),
        provider_id: configuration.providerId,
        model_id: configuration.modelId,
        reasoning_level: configuration.reasoningLevel,
        permissions: configuration.permissions,
        plan_enabled: configuration.planEnabled,
      },
    })) {
      this.pendingText = text;
      this.pendingDelivery = requestedDelivery;
      this.draft.set("");
      this.globalError.set("");
    }
  }

  submitOrStop(): void {
    const sessionId = this.current();
    if (sessionId && this.running()[sessionId]) {
      this.command({ Interrupt: { session_id: sessionId } });
      return;
    }
    this.send();
  }

  sendWithOppositeDelivery(): void {
    this.send(oppositeDelivery(this.config()?.default_delivery ?? "queue"));
  }

  clearTranscript(): void {
    const sessionId = this.current();
    if (!sessionId) {
      this.newTask();
      this.draft.set("");
      return;
    }
    this.items.update((all) => ({ ...all, [sessionId]: [] }));
    this.errors.update((all) => ({ ...all, [sessionId]: "" }));
  }

  answerApproval(decision: ApprovalDecision, feedback?: string): void {
    const sessionId = this.current();
    if (!sessionId) return;
    const approval = (this.approvals()[sessionId] ?? [])[0];
    if (!approval) return;
    if (this.command({
      ApprovalReply: { request_id: approval.requestId, decision, feedback: feedback ?? null },
    }, sessionId)) {
      this.approvals.update((all) => ({
        ...all,
        [sessionId]: (all[sessionId] ?? []).filter((item) => item.requestId !== approval.requestId),
      }));
    }
  }

  answerQuestion(answers: string[][] | null): void {
    const sessionId = this.current();
    if (!sessionId) return;
    const question = this.questions()[sessionId];
    if (!question) return;
    if (this.command({ QuestionReply: { request_id: question.requestId, answers } }, sessionId)) {
      this.questions.update((all) => ({ ...all, [sessionId]: null }));
    }
  }

  cancelQueued(id: string): void {
    this.queueCommand("CancelQueued", { id });
  }

  moveQueuedLane(id: string, steer: boolean): void {
    this.queueCommand("MoveQueuedLane", { id, steer });
  }

  reorderQueued(id: string, direction: -1 | 1): void {
    const queue = this.currentQueue();
    const lane = queue.steer.some((item) => item.id === id) ? queue.steer : queue.pending;
    const index = lane.findIndex((item) => item.id === id);
    const target = index + direction;
    if (index < 0 || target < 0 || target >= lane.length) return;
    this.queueCommand("ReorderQueued", { id, before_id: lane[target]?.id ?? null });
  }

  sendQueuedNow(id: string): void {
    this.queueCommand("SendQueuedNow", { id });
  }

  editQueued(item: QueueItem): void {
    if (this.queueCommand("CancelQueued", { id: item.id })) this.draft.set(item.text);
  }

  resumeQueue(): void {
    this.queueCommand("ResumeQueue", {});
  }

  resolveHeld(disposition: HeldDisposition): void {
    this.queueCommand("ResolveHeldQueue", { disposition });
  }

  private queueCommand(kind: string, data: Payload): boolean {
    const sessionId = this.current();
    return sessionId ? this.command({ [kind]: { session_id: sessionId, ...data } }, sessionId) : false;
  }

  private command(op: Payload, sessionId?: string): boolean {
    try {
      this.agent.send(JSON.stringify(op));
      this.globalError.set("");
      return true;
    } catch (error) {
      const message = errorMessage(error);
      if (sessionId) this.setError(sessionId, message);
      else this.globalError.set(message);
      return false;
    }
  }

  private updateConfiguration(changes: Partial<SessionConfiguration>): void {
    const sessionId = this.current();
    if (!sessionId) {
      this.heroConfiguration.update((configuration) => ({ ...configuration, ...changes }));
      return;
    }
    this.configurations.update((all) => ({
      ...all,
      [sessionId]: { ...(all[sessionId] ?? this.heroConfiguration()), ...changes },
    }));
  }

  private syncCurrentModelName(fallback = ""): void {
    const selected = this.selectedModel();
    this.currentModel.set(selected?.model.name || selected?.model.id || fallback);
  }

  saveThemePreference(theme: string, mode: "system" | "light" | "dark"): void {
    const config = this.config();
    if (config) this.saveConfig({ ...config, theme, theme_mode: mode });
  }

  saveConfig(config: AppConfig): boolean {
    return this.command({ SaveConfig: { config } });
  }

  upsertProvider(provider: ProviderConfig): boolean {
    const config = this.config();
    if (!config) return false;
    const exists = config.providers.some((item) => item.id === provider.id);
    return this.saveConfig({
      ...config,
      providers: exists
        ? config.providers.map((item) => item.id === provider.id ? provider : item)
        : [...config.providers, provider],
    });
  }

  deleteProvider(providerId: string): boolean {
    const config = this.config();
    if (!config) return false;
    const isDefault = config.default_provider === providerId;
    return this.saveConfig({
      ...config,
      default_provider: isDefault ? "" : config.default_provider,
      default_model: isDefault ? "" : config.default_model,
      providers: config.providers.filter((provider) => provider.id !== providerId),
    });
  }

  setProviderEnabled(providerId: string, enabled: boolean): boolean {
    const provider = this.config()?.providers.find((item) => item.id === providerId);
    return provider ? this.upsertProvider({ ...provider, enabled }) : false;
  }

  upsertModel(providerId: string, model: ProviderConfig["models"][number], originalId = model.id): boolean {
    const provider = this.config()?.providers.find((item) => item.id === providerId);
    if (!provider) return false;
    const exists = provider.models.some((item) => item.id === originalId);
    return this.upsertProvider({
      ...provider,
      models: exists
        ? provider.models.map((item) => item.id === originalId ? model : item)
        : [...provider.models, model],
    });
  }

  deleteModel(providerId: string, modelId: string): boolean {
    const config = this.config();
    const provider = config?.providers.find((item) => item.id === providerId);
    if (!config || !provider) return false;
    return this.saveConfig({
      ...config,
      default_model: config.default_provider === providerId && config.default_model === modelId
        ? ""
        : config.default_model,
      providers: config.providers.map((item) => item.id === providerId
        ? { ...item, models: item.models.filter((model) => model.id !== modelId) }
        : item),
    });
  }

  setModelEnabled(providerId: string, modelId: string, enabled: boolean): boolean {
    const model = this.config()?.providers.find((provider) => provider.id === providerId)
      ?.models.find((item) => item.id === modelId);
    return model ? this.upsertModel(providerId, { ...model, enabled }) : false;
  }

  setProviderIcon(providerId: string, icon: string | null): boolean {
    const provider = this.config()?.providers.find((item) => item.id === providerId);
    return provider ? this.upsertProvider({ ...provider, icon }) : false;
  }

  setModelIcon(providerId: string, modelId: string, icon: string | null): boolean {
    const model = this.config()?.providers.find((provider) => provider.id === providerId)
      ?.models.find((item) => item.id === modelId);
    return model ? this.upsertModel(providerId, { ...model, icon }) : false;
  }

  setDefaultModel(providerId: string, modelId: string): boolean {
    const config = this.config();
    return config ? this.saveConfig({ ...config, default_provider: providerId, default_model: modelId }) : false;
  }

  testProvider(providerId: string): boolean {
    if (this.testingProviders()[providerId]) return false;
    const sent = this.command({ TestProvider: { provider_id: providerId } });
    if (sent) this.testingProviders.update((all) => ({ ...all, [providerId]: true }));
    return sent;
  }

  setProviderApiKey(providerId: string, secret: string): boolean {
    return this.command({ SetProviderApiKey: { provider_id: providerId, secret } });
  }

  clearProviderApiKey(providerId: string): boolean {
    return this.command({ ClearProviderApiKey: { provider_id: providerId } });
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
        this.configurations.update((all) => ({
          ...all,
          [sessionId!]: {
            providerId: data.provider_id ?? null,
            modelId: data.model_id ?? null,
            reasoningLevel: data.reasoning_level ?? null,
            permissions: data.permissions ?? permissionsForMode("auto"),
            planEnabled: data.plan_enabled ?? false,
          },
        }));
        this.syncCurrentModelName(data.model || "");
        this.currentTitle.set(
          this.sessions().find((item) => item.id === sessionId)?.title ?? "当前会话",
        );
        const pending = this.pendingText;
        this.pendingText = null;
        if (pending) {
          this.command({
            SendMessage: {
              session_id: sessionId!,
              content: pending,
              files: [],
              images: [],
              permissions: this.configuration().permissions,
              delivery: this.pendingDelivery,
            },
          }, sessionId!);
        }
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
        this.syncCurrentModelName(this.currentModel());
        break;
      case "ProviderCredentials":
        this.providerCredentials.set(data.credentials ?? {});
        break;
      case "TestResult":
        this.testResults.update((all) => ({ ...all, [data.provider_id]: data.result }));
        this.testingProviders.update((all) => ({ ...all, [data.provider_id]: false }));
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
        this.upsertMarkdown(sessionId!, data.item_id, "markdown", data.full_text, false, false);
        break;
      case "ToolInputStart":
        this.upsertTool(sessionId!, data.item_id, { tool: data.tool, status: "input" });
        break;
      case "ToolInputDelta":
        this.upsertTool(sessionId!, data.item_id, { detail: data.delta }, true);
        break;
      case "ToolInputDiscard":
        this.updateItems(sessionId!, (items) => items.filter((item) => item.id !== data.item_id));
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
      case "QueueChanged":
        this.updateQueue(sessionId!, { pending: data.items });
        break;
      case "SteerChanged":
        this.updateQueue(sessionId!, { steer: data.items });
        break;
      case "QueueDrainChanged":
        this.updateQueue(sessionId!, {
          paused: data.auto_drain ? null : (data.reason ?? "error"),
        });
        break;
      case "HeldQueuePrompt":
        this.updateQueue(sessionId!, { held: data.text });
        break;
      case "HeldQueueResolved":
        this.updateQueue(sessionId!, { held: null });
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
                dangerKey: data.danger_key ?? null,
                requestedWriteRoots: data.requested_write_roots ?? [],
                availableDecisions: data.available_decisions ?? [],
              },
            ],
          };
        });
        break;
      case "QuestionRequested":
        this.questions.update((all) => ({
          ...all,
          [sessionId!]: { requestId: data.request_id, questions: data.questions },
        }));
        break;
      case "PermissionsChanged":
        this.updateConfigurationFor(sessionId!, { permissions: data.permissions });
        break;
      case "PlanModeChanged":
        this.updateConfigurationFor(sessionId!, { planEnabled: data.enabled });
        break;
      case "TurnStarted":
        this.startRate(sessionId!);
        this.setRunning(sessionId!, true);
        break;
      case "TurnComplete":
      case "TurnAborted":
        this.setRunning(sessionId!, false);
        this.approvals.update((all) => ({ ...all, [sessionId!]: [] }));
        this.questions.update((all) => ({ ...all, [sessionId!]: null }));
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

  private updateQueue(sessionId: string, changes: Partial<QueueState>): void {
    this.queues.update((all) => ({
      ...all,
      [sessionId]: { ...(all[sessionId] ?? EMPTY_QUEUE), ...changes },
    }));
  }

  private updateConfigurationFor(
    sessionId: string,
    changes: Partial<SessionConfiguration>,
  ): void {
    this.configurations.update((all) => ({
      ...all,
      [sessionId]: { ...(all[sessionId] ?? this.heroConfiguration()), ...changes },
    }));
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
