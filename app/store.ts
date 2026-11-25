import { batch, signal } from "nsh";
import { Agent } from "pig-code/native";
import { t } from "./i18n";
import {
  AppConfig,
  ApprovalDecision,
  ChatItem,
  ComposerImage,
  ConnectionTestResult,
  ContextBreakdown,
  ContextUsage,
  DeveloperConfig,
  EditDiff,
  HeldDisposition,
  GitDiffSnapshot,
  GitStatusSnapshot,
  MessageDelivery,
  ModelRegistryInfo,
  NoteItem,
  Payload,
  PendingApproval,
  PendingImage,
  PendingQuestion,
  PermissionSettings,
  PluginConfigItem,
  PluginContributionConflict,
  PluginPackageInfo,
  PluginCommand,
  PluginRuntimeContributions,
  PluginMarketplacePackage,
  PluginMarketplaceCatalog,
  PluginMarketplacePreview,
  PluginMarketplaceSource,
  PluginRuntimeState,
  PlatformSandboxStatus,
  ProcessSample,
  ProjectHooksNotice,
  RetryStatus,
  RightPanelRequest,
  SandboxConfig,
  ProviderConfig,
  ProviderCredentialStatus,
  QueueItem,
  QueueState,
  SessionConfiguration,
  SessionContentHit,
  SessionMeta,
  WorkspaceMeta,
  SubagentView,
  TaskSummary,
  TodoItem,
  ToolItem,
  TurnState,
  errorMessage,
  eventKind,
} from "./protocol";
import { oppositeDelivery, parseSlashCommand, permissionsForMode } from "./commands";
import { composerImage, updateImageMentionsAfterRemoval } from "./media";
import { PluginThemeDefinition } from "./themes";
import { PluginSequencer, pluginInstance, validatePluginEnvelope } from "./plugins/runtime";
import { pluginProviderConfigs } from "./plugins/providers";

let generatedId = 0;

const EMPTY_QUEUE: QueueState = { pending: [], steer: [], paused: null, held: null };

type PendingSubmission = {
  text: string;
  files: string[];
  images: PendingImage[];
  delivery: MessageDelivery;
};

export class AgentStore {
  readonly agent: Agent;
  readonly cwd;
  readonly heroCwd = signal<string | null>(null);
  readonly draft = signal("");
  readonly draftFiles = signal<string[]>([]);
  readonly draftImages = signal<ComposerImage[]>([]);
  readonly threadSearchOpen = signal(false);
  readonly sessions = signal<SessionMeta[]>([]);
  readonly workspaces = signal<WorkspaceMeta[]>([]);
  readonly current = signal<string | null>(null);
  readonly currentTitle = signal(t("store.new_task"));
  readonly currentModel = signal("");
  readonly items = signal<Record<string, ChatItem[]>>({});
  readonly turns = signal<Record<string, TurnState[]>>({});
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
  readonly contextUsage = signal<Record<string, ContextUsage>>({});
  readonly contextBreakdowns = signal<Record<string, ContextBreakdown>>({});
  readonly outputRate = signal<Record<string, number>>({});
  readonly expandedItems = signal<Record<string, boolean>>({});
  readonly expandedTurns = signal<Record<string, boolean>>({});
  readonly copiedItems = signal<Record<string, boolean>>({});
  readonly fileSearchResults = signal<{ query: string; results: string[] }>({ query: "", results: [] });
  readonly todos = signal<Record<string, TodoItem[]>>({});
  readonly tasks = signal<Record<string, TaskSummary[]>>({});
  readonly sessionChanges = signal<Record<string, Record<string, EditDiff>>>({});
  readonly retrying = signal<Record<string, RetryStatus | null>>({});
  readonly hookRunning = signal<Record<string, { event: string; label: string } | null>>({});
  readonly projectHooks = signal<Record<string, ProjectHooksNotice | null>>({});
  readonly subagents = signal<Record<string, SubagentView>>({});
  private agentFinishSeq = 0;
  readonly rightPanelRequest = signal<RightPanelRequest | null>(null);
  readonly config = signal<AppConfig | null>(null);
  readonly gitStatus = signal<GitStatusSnapshot | null>(null);
  readonly gitDiff = signal<GitDiffSnapshot | null>(null);
  readonly providerCredentials = signal<Record<string, ProviderCredentialStatus>>({});
  readonly testResults = signal<Record<string, ConnectionTestResult>>({});
  readonly testingProviders = signal<Record<string, boolean>>({});
  readonly modelInfo = signal<Record<string, ModelRegistryInfo | null>>({});
  readonly globalError = signal("");
  readonly pluginPackages = signal<PluginPackageInfo[]>([]);
  readonly pluginCatalog = signal<Record<string, PluginMarketplacePackage[]>>({});
  readonly pluginMarketplaces = signal<PluginMarketplaceSource[]>([]);
  readonly activePluginMarketplace = signal("official");
  readonly pluginMarketplaceLoading = signal<Record<string, boolean>>({});
  readonly pluginMarketplaceErrors = signal<Record<string, string>>({});
  readonly pluginMarketplaceInstalling = signal<{ sourceId: string; packageId: string } | null>(null);
  readonly pluginConflicts = signal<PluginContributionConflict[]>([]);
  private readonly pluginConfigPending = new Map<string, {
    resolve: (value: unknown) => void;
    reject: (error: unknown) => void;
  }>();
  private pluginConfigSeq = 0;
  private readonly pluginSecretPending = new Map<string, {
    resolve: (value: unknown) => void;
    reject: (error: unknown) => void;
  }>();
  private pluginSecretSeq = 0;
  readonly pluginRuntimeContributions = signal<PluginRuntimeContributions>({});
  readonly pluginRuntime = signal<Record<string, PluginRuntimeState>>({});
  readonly pluginOrder = signal<string[]>([]);
  readonly pluginNavigation = signal<string | null>(null);
  readonly pluginLogs = signal<Record<string, unknown[]>>({});
  readonly pluginThemes = signal<PluginThemeDefinition[]>([]);
  readonly fontFamilies = signal<string[]>([]);
  readonly themeError = signal("");
  private autoOpen = true;
  private heroModelDirty = false;
  private pendingSubmission: PendingSubmission | null = null;
  private activeTurns = new Map<string, string>();
  private configuredSessions = new Set<string>();
  private imageSourceCache = new Map<string, string>();
  private themeLoadId = 0;
  private pluginGeneration = 0;
  private pluginSequencers = new Map<string, { epoch: number; sequences: PluginSequencer }>();
  private rateSession: string | null = null;
  private rateStartedAt = 0;
  private rateCharacters = 0;
  private rateRenderedAt = 0;

  constructor(selftest = false) {
    this.agent = new Agent(null, selftest);
    this.cwd = signal(this.agent.cwd);
  }

  start(): void {
    try {
      this.fontFamilies.set(this.agent.listFonts());
    } catch {
      this.fontFamilies.set([]);
    }
    this.refreshPluginMarketplaces();
    void this.agent.listen((raw) => {
      try {
        const events = JSON.parse(raw);
        batch(() => {
          for (const event of events) this.reduce(event);
        });
      } catch (error) {
        this.globalError.set(t("store.error.event_parse", { error: errorMessage(error) }));
      }
    });
    void this.agent.listenPlugins((raw) => {
      try {
        const message = JSON.parse(raw);
        if (!validatePluginEnvelope(message)) throw new Error(t("store.error.invalid_plugin_envelope"));
        const instance = message.instance;
        let sequencer = this.pluginSequencers.get(instance);
        if (!sequencer || message.epoch > sequencer.epoch) {
          sequencer = { epoch: message.epoch, sequences: new PluginSequencer() };
          this.pluginSequencers.set(instance, sequencer);
        }
        if (message.epoch < sequencer.epoch || !sequencer.sequences.accept(message.stream, message.sequence)) return;
        if (message.kind === "runtime") {
          const runtime = message.body as PluginRuntimeState;
          const current = this.pluginRuntime()[instance];
          if (current && (current.epoch > runtime.epoch ||
            (current.epoch === runtime.epoch && current.generation > runtime.generation))) return;
          this.pluginRuntime.update((instances) => ({ ...instances, [instance]: runtime }));
          if (runtime.state === "active" || runtime.state === "failed" || runtime.state === "stopped") {
            void this.loadPluginThemes(this.pluginPackages());
          }
          if (runtime.state === "failed") this.globalError.set(t("store.error.plugin_runtime", { message: runtime.message ?? t("store.error.unknown") }));
        } else if (message.kind === "host.action") {
          const action = message.body as Record<string, any>;
          if (action.name === "composer.submit") this.send();
          if (action.name === "navigation.open" && typeof action.pageId === "string") {
            this.pluginNavigation.set(action.pageId);
          }
        } else if (message.kind === "log.event") {
          this.pluginLogs.update((logs) => ({
            ...logs,
            [instance]: [...(logs[instance] ?? []), message.body].slice(-100),
          }));
        }
      } catch (error) {
        this.globalError.set(t("store.error.plugin_event_parse", { error: errorMessage(error) }));
      }
    });
  }

  newTask(): void {
    this.autoOpen = false;
    this.pendingSubmission = null;
    this.current.set(null);
    this.heroCwd.set(null);
    this.heroModelDirty = false;
    this.currentTitle.set(t("store.new_task"));
    this.currentModel.set("");
    this.applyHeroDefaults();
    this.globalError.set("");
  }

  selectHeroWorkspace(cwd: string | null): void {
    this.autoOpen = false;
    this.heroCwd.set(cwd);
    this.applyHeroDefaults();
  }

  refreshGitStatus(): boolean {
    const diff = this.gitDiff();
    if (diff) this.openGitDiff(diff.path, diff.staged);
    return this.command({ GitStatus: { cwd: this.cwd() } });
  }

  openGitDiff(path: string, staged: boolean): boolean {
    return this.command({ GitDiff: { cwd: this.cwd(), path, staged } });
  }

  closeGitDiff(): void {
    this.gitDiff.set(null);
  }

  private applyHeroDefaults(): void {
    const cwd = this.heroCwd() ?? this.cwd();
    const seed = this.sessions()
      .filter((session) => !session.archived && session.cwd === cwd)
      .sort((a, b) => b.updated_at - a.updated_at)[0];
    if (seed) {
      this.heroConfiguration.update((configuration) => ({
        ...configuration,
        permissions: seed.permissions ?? configuration.permissions,
        reasoningLevel: seed.reasoning_level ?? null,
        ...(this.heroModelDirty ? {} : {
          providerId: seed.provider_id ?? null,
          modelId: seed.model_id ?? null,
        }),
      }));
    }
    this.syncCurrentModelName();
  }

  openSession(sessionId: string): void {
    this.autoOpen = false;
    this.pendingSubmission = null;
    try {
      this.agent.openSession(sessionId);
    } catch (error) {
      this.globalError.set(errorMessage(error));
    }
  }

  newTaskInWorkspace(path: string): void {
    this.newTask();
    this.selectHeroWorkspace(path);
  }

  setSessionPinned(sessionId: string, pinned: boolean): void {
    this.updateSessionMeta(sessionId, { pinned });
  }

  setSessionArchived(sessionId: string, archived: boolean): void {
    this.updateSessionMeta(sessionId, { archived });
  }

  renameSession(sessionId: string, title: string): void {
    if (this.updateSessionMeta(sessionId, { title }) && this.current() === sessionId) {
      this.currentTitle.set(title);
    }
  }

  deleteSession(sessionId: string): void {
    if (this.command({ DeleteSession: { session_id: sessionId } }) && this.current() === sessionId) {
      this.newTask();
    }
  }

  removeWorkspace(path: string): void {
    this.command({ RemoveWorkspace: { path } });
  }

  renameWorkspace(path: string, alias: string | null): void {
    this.command({ RenameWorkspace: { path, alias } });
  }

  private updateSessionMeta(
    sessionId: string,
    changes: { pinned?: boolean; archived?: boolean; title?: string },
  ): boolean {
    return this.command({
      UpdateSessionMeta: {
        session_id: sessionId,
        pinned: changes.pinned ?? null,
        archived: changes.archived ?? null,
        title: changes.title ?? null,
      },
    });
  }

  configuration(): SessionConfiguration {
    const sessionId = this.current();
    return (sessionId && this.configurations()[sessionId]) || this.heroConfiguration();
  }

  currentQueue(): QueueState {
    const sessionId = this.current();
    return (sessionId && this.queues()[sessionId]) || EMPTY_QUEUE;
  }

  /** Providers contributed by enabled packages. They are runtime-only: the
   * persisted config never contains them, and their ids are namespaced so a
   * plugin cannot shadow a user provider. The backend applies the same rules
   * when it resolves a session model. */
  pluginProviders(): ProviderConfig[] {
    return pluginProviderConfigs(this.pluginPackages(), this.pluginRuntimeContributions());
  }

  modelOptions() {
    return [...(this.config()?.providers ?? []), ...this.pluginProviders()].flatMap((provider) =>
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
    const sessionId = this.current();
    if (!sessionId) this.heroModelDirty = true;
    this.updateConfiguration({ providerId, modelId, reasoningLevel });
    this.syncCurrentModelName();
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
    const files = this.draftFiles();
    const images = this.draftImages().map(({ bytes, mime }) => ({ bytes, mime }));
    if (!text && images.length === 0) return;
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
        this.globalError.set(t("store.error.stop_turn_first"));
        return;
      }
      if (slash.name === "/clear") {
        this.clearDraft();
        this.clearTranscript();
        return;
      }
      if (!sessionId) {
        this.globalError.set(t("store.error.send_first"));
        return;
      }
      if (this.command({ Compact: { session_id: sessionId, instruction: slash.instruction ?? null } })) {
        this.clearDraft();
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
          files,
          images,
          permissions: configuration.permissions,
          delivery: running ? requestedDelivery : "queue",
        },
      })) {
        this.clearDraft();
      }
      return;
    }
    if (this.command({
      NewSession: {
        cwd: this.heroCwd() ?? this.cwd(),
        provider_id: configuration.providerId,
        model_id: configuration.modelId,
        reasoning_level: configuration.reasoningLevel,
        permissions: configuration.permissions,
        plan_enabled: configuration.planEnabled,
      },
    })) {
      this.pendingSubmission = { text, files, images, delivery: requestedDelivery };
      this.clearDraft();
      this.globalError.set("");
    }
  }

  clearDraft(): void {
    this.draft.set("");
    this.draftFiles.set([]);
    this.draftImages.set([]);
    this.fileSearchResults.set({ query: "", results: [] });
  }

  addDraftFile(path: string): void {
    if (!this.draftFiles().includes(path)) this.draftFiles.update((files) => [...files, path]);
  }

  removeDraftFile(path: string): void {
    this.draftFiles.update((files) => files.filter((file) => file !== path));
  }

  addDraftImages(images: ComposerImage[]): void {
    this.draftImages.update((current) => [...current, ...images].slice(0, 8));
  }

  removeDraftImage(id: string): void {
    const images = this.draftImages();
    const index = images.findIndex((image) => image.id === id);
    if (index < 0) return;
    this.draftImages.set(images.filter((image) => image.id !== id));
    this.draft.update((text) => updateImageMentionsAfterRemoval(text, index + 1, images.length));
  }

  searchFiles(query: string): void {
    const sessionId = this.current();
    this.command({
      SearchFiles: {
        session_id: sessionId ?? "",
        query,
        cwd: sessionId ? null : (this.heroCwd() ?? this.cwd()),
      },
    });
  }

  async searchSessionContent(query: string): Promise<SessionContentHit[]> {
    return JSON.parse(await this.agent.searchSessionContent(query)) as SessionContentHit[];
  }

  stop(): void {
    const sessionId = this.current();
    if (sessionId) this.command({ Interrupt: { session_id: sessionId } }, sessionId);
  }

  sendWithOppositeDelivery(): void {
    this.send(oppositeDelivery(this.config()?.default_delivery ?? "queue"));
  }

  clearTranscript(): void {
    const sessionId = this.current();
    if (!sessionId) {
      this.newTask();
      this.clearDraft();
      return;
    }
    this.items.update((all) => ({ ...all, [sessionId]: [] }));
    this.turns.update((all) => ({ ...all, [sessionId]: [] }));
    this.sessionChanges.update((all) => ({ ...all, [sessionId]: {} }));
    this.activeTurns.delete(sessionId);
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
      this.updateItems(sessionId, (items) => items.map((item) =>
        item.kind === "tool" && item.approvalPending ? { ...item, approvalPending: false } : item
      ));
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
    this.reorderQueuedBy(id, direction);
  }

  reorderQueuedBy(id: string, offset: number): void {
    const queue = this.currentQueue();
    const lane = queue.steer.some((item) => item.id === id) ? queue.steer : queue.pending;
    const index = lane.findIndex((item) => item.id === id);
    if (index < 0 || offset === 0) return;
    const target = Math.max(0, Math.min(lane.length - 1, index + offset));
    if (target === index) return;
    const beforeId = target < index ? lane[target]?.id ?? null : lane[target + 1]?.id ?? null;
    this.queueCommand("ReorderQueued", { id, before_id: beforeId });
  }

  sendQueuedNow(id: string): void {
    this.queueCommand("SendQueuedNow", { id });
  }

  editQueued(item: QueueItem): void {
    if (this.queueCommand("CancelQueued", { id: item.id })) {
      this.draft.set(item.text);
      this.draftImages.set((item.images ?? []).map(composerImage));
    }
  }

  reorderQueuedBefore(id: string, beforeId: string | null): void {
    this.queueCommand("ReorderQueued", { id, before_id: beforeId });
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

  currentTurns(): TurnState[] {
    const id = this.current();
    return id ? (this.turns()[id] ?? []) : [];
  }

  currentTodos(): TodoItem[] {
    const id = this.current();
    return id ? (this.todos()[id] ?? []) : [];
  }

  currentTasks(): TaskSummary[] {
    const id = this.current();
    return id ? (this.tasks()[id] ?? []) : [];
  }

  currentChanges(): EditDiff[] {
    const id = this.current();
    return id ? Object.values(this.sessionChanges()[id] ?? {}) : [];
  }

  currentUsage(): ContextUsage | undefined {
    const id = this.current();
    return id ? this.contextUsage()[id] : undefined;
  }

  currentBreakdown(): ContextBreakdown | undefined {
    const id = this.current();
    return id ? this.contextBreakdowns()[id] : undefined;
  }

  sessionTotals(): {
    turns: number;
    steps: number;
    output: number;
    decodeMs: number;
    apiMs: number;
    toolMs: number;
    ttftMs: number;
    ttftSteps: number;
  } {
    const totals = { turns: 0, steps: 0, output: 0, decodeMs: 0, apiMs: 0, toolMs: 0, ttftMs: 0, ttftSteps: 0 };
    for (const turn of this.currentTurns()) {
      if (turn.status === "running" || ((turn.durationMs ?? 0) === 0 && !turn.stats)) continue;
      totals.turns += 1;
      const stats = turn.stats;
      if (!stats) continue;
      totals.steps += stats.api_steps;
      totals.apiMs += stats.api_ms;
      totals.toolMs += stats.tool_ms;
      totals.ttftMs += stats.ttft_ms;
      totals.ttftSteps += stats.ttft_steps;
      const decodeMs = stats.api_ms - stats.ttft_ms;
      if (stats.ttft_ms > 0 && decodeMs > 0) {
        totals.output += stats.output;
        totals.decodeMs += decodeMs;
      }
    }
    return totals;
  }

  isExpanded(id: string): boolean {
    return this.expandedItems()[id] ?? false;
  }

  toggleExpanded(id: string): void {
    this.expandedItems.update((all) => ({ ...all, [id]: !this.isExpanded(id) }));
  }

  isTurnExpanded(id: string): boolean {
    return this.expandedTurns()[id] ?? false;
  }

  toggleTurnExpanded(id: string): void {
    this.expandedTurns.update((all) => ({ ...all, [id]: !this.isTurnExpanded(id) }));
  }

  markCopied(id: string): void {
    this.copiedItems.update((all) => ({ ...all, [id]: true }));
    setTimeout(() => this.copiedItems.update((all) => ({ ...all, [id]: false })), 1200);
  }

  openChanges(): void {
    this.refreshGitStatus();
    this.rightPanelRequest.set({ kind: "changes", key: `changes-${Date.now()}` });
  }

  openFile(path: string, line?: number): void {
    this.rightPanelRequest.set({
      kind: "file",
      key: `file-${path}-${line ?? 0}-${Date.now()}`,
      path,
      cwd: this.cwd(),
      line,
    });
  }

  openCompactSummary(text: string): void {
    this.rightPanelRequest.set({ kind: "compact", key: `compact-${Date.now()}`, text });
  }

  openSubagent(agentId: string): void {
    const sessionId = this.current();
    if (!sessionId) return;
    this.command({ LoadSubagent: { session_id: sessionId, agent_id: agentId } }, sessionId);
    this.rightPanelRequest.set({ kind: "subagent", key: `agent-${agentId}-${Date.now()}`, sessionId, agentId });
  }

  openTask(task: TaskSummary): void {
    this.rightPanelRequest.set({ kind: "task", key: `task-${task.id}-${Date.now()}`, task });
  }

  readFile(path: string, cwd: string): { content: string; truncated: boolean } {
    return JSON.parse(this.agent.readFile(path, cwd)) as { content: string; truncated: boolean };
  }

  sessionImageSource(sessionId: string, imageNumber: number): string {
    const key = `${sessionId}:${imageNumber}`;
    const cached = this.imageSourceCache.get(key);
    if (cached !== undefined) return cached;
    try {
      const source = this.agent.sessionImageSource(sessionId, imageNumber);
      this.imageSourceCache.set(key, source);
      return source;
    } catch {
      this.imageSourceCache.set(key, "");
      return "";
    }
  }

  forkTurn(turnId: string): void {
    const sessionId = this.current();
    if (!sessionId) return;
    const index = this.currentTurns().findIndex((turn) => turn.id === turnId);
    if (index >= 0) this.command({ ForkSession: { session_id: sessionId, turns: index + 1 } }, sessionId);
  }

  recallLastMessage(): void {
    const sessionId = this.current();
    if (!sessionId || this.running()[sessionId]) return;
    const items = this.items()[sessionId] ?? [];
    const user = items.findLast((item) => item.kind === "user");
    if (!user || user.kind !== "user") return;
    this.draft.set(user.text);
    this.draftFiles.set(user.files);
    const images = user.imageNums.flatMap((imageNumber) => {
      try {
        const image = JSON.parse(this.agent.sessionImage(sessionId, imageNumber)) as PendingImage;
        return [{
          ...image,
          id: `recalled-${imageNumber}`,
          preview: this.agent.sessionImageSource(sessionId, imageNumber),
        }];
      } catch {
        return [];
      }
    });
    this.draftImages.set(images);
    this.command({ RecallLastMessage: { session_id: sessionId } }, sessionId);
  }

  retryNow(): void {
    const sessionId = this.current();
    if (sessionId) this.command({ RetryNow: { session_id: sessionId } }, sessionId);
  }

  trustProjectHooks(): void {
    const sessionId = this.current();
    if (sessionId) this.command({ TrustProjectHooks: { session_id: sessionId } }, sessionId);
  }

  dismissProjectHooks(): void {
    const sessionId = this.current();
    if (sessionId) this.projectHooks.update((all) => ({ ...all, [sessionId]: null }));
  }

  revertFile(path: string): void {
    const sessionId = this.current();
    if (sessionId) this.command({ RevertFile: { session_id: sessionId, path } }, sessionId);
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

  saveLanguage(language: "system" | "zh-CN" | "en"): void {
    const config = this.config();
    if (config) this.saveConfig({ ...config, language: language === "system" ? null : language });
  }

  saveFonts(ui: string, mono: string): void {
    const config = this.config();
    if (config) this.saveConfig({ ...config, ui_font: ui.trim() || null, mono_font: mono.trim() || null });
  }

  saveDeveloperOptions(options: Partial<DeveloperConfig>): void {
    const config = this.config();
    if (config) this.saveConfig({ ...config, developer: { ...config.developer, ...options } });
  }

  saveSandboxOptions(options: Partial<SandboxConfig>): void {
    const config = this.config();
    if (config) this.saveConfig({ ...config, sandbox: { ...config.sandbox, ...options } });
  }

  sandboxStatus(): PlatformSandboxStatus | null {
    try {
      const parsed = JSON.parse(this.agent.sandboxStatus()) as Partial<PlatformSandboxStatus>;
      if (
        typeof parsed.enabled === "boolean" &&
        typeof parsed.supported === "boolean" &&
        typeof parsed.backend === "string"
      ) {
        return {
          enabled: parsed.enabled,
          supported: parsed.supported,
          backend: parsed.backend,
          detail: parsed.detail ?? null,
        };
      }
      return null;
    } catch {
      return null;
    }
  }

  processSample(): ProcessSample {
    try {
      return JSON.parse(this.agent.processSample()) as ProcessSample;
    } catch {
      return {};
    }
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

  lookupModel(id: string): boolean {
    return this.command({ ModelLookup: { id } });
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

  async installPlugin(packagePath: string, rootPath: string): Promise<void> {
    try {
      await this.agent.installPlugin(packagePath, rootPath);
    } catch (error) {
      this.globalError.set(errorMessage(error));
      throw error;
    }
  }

  uninstallPlugin(plugin: PluginPackageInfo): void {
    try {
      this.agent.uninstallPlugin(plugin.id, plugin.digest);
    } catch (error) {
      this.globalError.set(errorMessage(error));
      throw error;
    }
  }

  refreshPluginMarketplaces(): void {
    try {
      const sources = JSON.parse(this.agent.marketplaceSources()) as PluginMarketplaceSource[];
      this.pluginMarketplaces.set(sources);
    } catch (error) {
      this.globalError.set(errorMessage(error));
    }
  }

  selectPluginMarketplace(id: string): void {
    if (this.pluginMarketplaces().some((source) => source.id === id)) {
      this.activePluginMarketplace.set(id);
    }
  }

  async previewPluginMarketplace(indexUrl: string): Promise<PluginMarketplacePreview> {
    const raw = await this.agent.previewMarketplace(indexUrl);
    return JSON.parse(raw) as PluginMarketplacePreview;
  }

  addPluginMarketplace(preview: PluginMarketplacePreview): string {
    const sources = JSON.parse(this.agent.addMarketplace(JSON.stringify(preview))) as PluginMarketplaceSource[];
    this.pluginMarketplaces.set(sources);
    return sources.find((source) => source.index_url === preview.index_url)?.id ?? preview.id;
  }

  removePluginMarketplace(id: string): void {
    const sources = JSON.parse(this.agent.removeMarketplace(id)) as PluginMarketplaceSource[];
    this.pluginMarketplaces.set(sources);
    this.pluginCatalog.update((all) => {
      const next = { ...all };
      delete next[id];
      return next;
    });
    this.pluginMarketplaceLoading.update((all) => {
      const next = { ...all };
      delete next[id];
      return next;
    });
    this.pluginMarketplaceErrors.update((all) => {
      const next = { ...all };
      delete next[id];
      return next;
    });
    if (this.activePluginMarketplace() === id) this.activePluginMarketplace.set("official");
  }

  async refreshMarketplace(sourceId = this.activePluginMarketplace()): Promise<void> {
    const source = this.pluginMarketplaces().find((item) => item.id === sourceId);
    if (!source || this.pluginMarketplaceLoading()[sourceId]) return;
    this.pluginMarketplaceLoading.update((all) => ({ ...all, [sourceId]: true }));
    this.pluginMarketplaceErrors.update((all) => ({ ...all, [sourceId]: "" }));
    try {
      const raw = await this.agent.marketplaceCatalog(source.index_url);
      const catalog = JSON.parse(raw) as PluginMarketplaceCatalog;
      if (!this.pluginMarketplaces().some((item) => item.id === sourceId)) return;
      this.pluginCatalog.update((all) => ({ ...all, [sourceId]: catalog.packages }));
    } catch (error) {
      if (!this.pluginMarketplaces().some((item) => item.id === sourceId)) return;
      this.pluginMarketplaceErrors.update((all) => ({ ...all, [sourceId]: errorMessage(error) }));
    } finally {
      if (this.pluginMarketplaces().some((item) => item.id === sourceId)) {
        this.pluginMarketplaceLoading.update((all) => ({ ...all, [sourceId]: false }));
      }
    }
  }

  async installMarketplacePlugin(
    plugin: PluginMarketplacePackage,
    sourceId = this.activePluginMarketplace(),
  ): Promise<void> {
    const source = this.pluginMarketplaces().find((item) => item.id === sourceId);
    if (!source || this.pluginMarketplaceInstalling()) return;
    this.pluginMarketplaceInstalling.set({ sourceId, packageId: plugin.id });
    this.pluginMarketplaceErrors.update((all) => ({ ...all, [sourceId]: "" }));
    try {
      await this.agent.installMarketplacePlugin(JSON.stringify(plugin), source.index_url, null);
    } catch (error) {
      this.pluginMarketplaceErrors.update((all) => ({ ...all, [sourceId]: errorMessage(error) }));
    } finally {
      this.pluginMarketplaceInstalling.set(null);
    }
  }

  readPluginConfig(
    plugin: PluginPackageInfo,
    key: string,
    sessionId?: string | null,
  ): Promise<unknown> {
    return this.pluginConfigRequest(plugin, { kind: "read_config", data: { path: key } }, sessionId);
  }

  writePluginConfig(
    plugin: PluginPackageInfo,
    key: string,
    value: unknown,
    sessionId?: string | null,
  ): Promise<unknown> {
    return this.pluginConfigRequest(
      plugin,
      { kind: "write_config", data: { path: key, value } },
      sessionId,
    );
  }

  private pluginConfigRequest(
    plugin: PluginPackageInfo,
    action: { kind: "read_config" | "write_config"; data: { path: string; value?: unknown } },
    sessionId?: string | null,
  ): Promise<unknown> {
    const requestId = `plugin-config-${++this.pluginConfigSeq}`;
    return new Promise((resolve, reject) => {
      this.pluginConfigPending.set(requestId, { resolve, reject });
      const sent = this.command({
        PluginAction: {
          request_id: requestId,
          package: plugin.id,
          digest: plugin.digest,
          action,
          session_id: sessionId ?? null,
        },
      }, sessionId ?? undefined);
      if (!sent) {
        this.pluginConfigPending.delete(requestId);
        reject(new Error(t("store.agent_unavailable")));
      }
    });
  }

  /** Store a package's owned credential (a provider key) in the host
   * credential store. The plugin can use it but never reads it back. */
  setPluginSecret(plugin: PluginPackageInfo, credential: string, secret: string): Promise<unknown> {
    return this.pluginSecretRequest(plugin, credential, secret);
  }

  clearPluginSecret(plugin: PluginPackageInfo, credential: string): Promise<unknown> {
    return this.pluginSecretRequest(plugin, credential, null);
  }

  private pluginSecretRequest(
    plugin: PluginPackageInfo,
    credential: string,
    secret: string | null,
  ): Promise<unknown> {
    const requestId = `plugin-secret-${++this.pluginSecretSeq}`;
    return new Promise((resolve, reject) => {
      this.pluginSecretPending.set(requestId, { resolve, reject });
      const sent = this.command(
        secret === null
          ? {
            ClearPluginSecret: {
              request_id: requestId,
              package: plugin.id,
              digest: plugin.digest,
              credential,
            },
          }
          : {
            SetPluginSecret: {
              request_id: requestId,
              package: plugin.id,
              digest: plugin.digest,
              credential,
              secret,
            },
          },
      );
      if (!sent) {
        this.pluginSecretPending.delete(requestId);
        reject(new Error(t("store.agent_unavailable")));
      }
    });
  }

  setPluginOrder(order: string[]): void {
    this.pluginOrder.set([...new Set(order)]);
  }

  pluginCommands(): Array<{
    instance: string;
    title: string;
    command: PluginCommand;
  }> {
    return this.pluginPackages()
      .filter((pkg) => pkg.enabled && this.pluginRuntime()[pluginInstance(pkg)]?.state === "active")
      .flatMap((pkg) => {
        const instance = pluginInstance(pkg);
        const dynamic = this.pluginRuntimeContributions()[`${pkg.id}:${pkg.digest}`]?.commands ?? [];
        // A backend may re-register a manifest command with a runtime
        // definition; the runtime one wins instead of listing it twice.
        const byId = new Map<string, PluginCommand>();
        for (const command of pkg.contributions.commands ?? []) byId.set(command.id, command);
        for (const command of dynamic) byId.set(command.id, command);
        return [...byId.values()]
          .filter((command) => command.enabled)
          .map((command) => ({
            instance,
            command,
            title: command.title["zh-CN"] ?? command.title.en ?? command.id,
          }));
      })
      .sort((a, b) => a.command.order - b.command.order || a.command.id.localeCompare(b.command.id));
  }

  runPluginCommand(instance: string, commandId: string): void {
    try {
      this.agent.dispatchPluginCommand(instance, commandId);
    } catch (error) {
      this.globalError.set(errorMessage(error));
    }
  }

  retryPlugin(plugin: PluginPackageInfo): void {
    try {
      this.agent.retryPlugin(plugin.id, plugin.digest);
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
      const runtime = this.pluginRuntime();
      const contributions = this.pluginRuntimeContributions();
      const projected = packages.map((pkg) => {
        const dynamic = contributions[`${pkg.id}:${pkg.digest}`] ?? {};
        const needsEngine = pkg.backend != null;
        // A runtime theme replaces the manifest theme of the same id.
        const themes = new Map<string, { id: string; entry: string }>();
        for (const theme of pkg.contributions.themes ?? []) themes.set(theme.id, theme);
        for (const theme of dynamic.themes ?? []) themes.set(theme.id, theme);
        return {
          ...pkg,
          enabled: pkg.enabled && (!needsEngine || runtime[pluginInstance(pkg)]?.state === "active"),
          contributions: {
            ...pkg.contributions,
            themes: [...themes.values()],
          },
        };
      });
      const raw = await this.agent.loadThemes(JSON.stringify(projected));
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
      case "WorkspaceList":
        this.workspaces.set(data.workspaces as WorkspaceMeta[]);
        break;
      case "SessionList": {
        const sessions = (data.sessions as SessionMeta[]).filter((item) => !item.archived);
        this.sessions.set(sessions);
        if (this.autoOpen && !this.current() && sessions.length > 0) {
          this.autoOpen = false;
          this.agent.openSession(sessions[0].id);
        }
        break;
      }
      case "ConversationSnapshot": {
        const agentId = typeof data.agent_id === "string" ? data.agent_id : null;
        const runId = typeof data.run_id === "string" ? data.run_id : null;
        const items = (data.items ?? []) as ChatItem[];
        if (agentId) {
          const key = `${sessionId!}:${agentId}`;
          this.subagents.update((all) => {
            const previous = all[key];
            if (runId && previous?.runId && previous.runId !== runId) return all;
            return {
              ...all,
              [key]: {
                sessionId: sessionId!,
                agentId,
                runId: runId ?? previous?.runId ?? null,
                title: data.title ?? previous?.title ?? "",
                subtitle: data.subtitle ?? previous?.subtitle ?? "",
                items: previous ? this.mergeConversationSnapshot(items, previous.items) : items,
                running: data.running ?? previous?.running ?? false,
              },
            };
          });
        } else {
          this.items.update((all) => ({ ...all, [sessionId!]: items }));
          this.setRunning(sessionId!, data.running ?? false);
        }
        break;
      }
      case "ConversationUpdate": {
        const agentId = typeof data.agent_id === "string" ? data.agent_id : null;
        const runId = typeof data.run_id === "string" ? data.run_id : null;
        const item = data.item as ChatItem;
        if (agentId) {
          const key = `${sessionId!}:${agentId}`;
          this.subagents.update((all) => {
            const previous = all[key] ?? {
              sessionId: sessionId!,
              agentId,
              title: "",
              subtitle: "",
              items: [],
              running: true,
              runId,
            };
            if (runId && previous.runId && previous.runId !== runId) return all;
            return {
              ...all,
              [key]: {
                ...previous,
                runId: runId ?? previous.runId ?? null,
                items: this.upsertConversationItems(previous.items, item),
              },
            };
          });
        } else {
          this.updateItems(sessionId!, (items) => this.upsertConversationItems(items, item));
        }
        break;
      }
      case "ConversationRemove": {
        const agentId = typeof data.agent_id === "string" ? data.agent_id : null;
        const runId = typeof data.run_id === "string" ? data.run_id : null;
        const ids = new Set<string>((data.item_ids ?? []) as string[]);
        if (agentId) {
          const key = `${sessionId!}:${agentId}`;
          this.subagents.update((all) => {
            const previous = all[key];
            if (!previous || (runId && previous.runId && previous.runId !== runId)) return all;
            return {
              ...all,
              [key]: { ...previous, items: previous.items.filter((item) => !ids.has(item.id)) },
            };
          });
        } else {
          this.updateItems(sessionId!, (items) => items.filter((item) => !ids.has(item.id)));
        }
        break;
      }
      case "ConversationStatus": {
        const key = `${sessionId!}:${data.agent_id}`;
        const runId = typeof data.run_id === "string" ? data.run_id : null;
        const running = data.running === true;
        this.subagents.update((all) => {
          const previous = all[key];
          if (!previous || (!running && runId && previous.runId && previous.runId !== runId)) {
            return all;
          }
          return {
            ...all,
            [key]: {
              ...previous,
              runId: runId ?? previous.runId ?? null,
              running,
            },
          };
        });
        break;
      }
      case "SessionConfigured": {
        this.autoOpen = false;
        this.current.set(sessionId);
        this.cwd.set(data.cwd);
        const cold = !this.configuredSessions.has(sessionId!);
        this.configuredSessions.add(sessionId!);
        if (cold) {
          this.activeTurns.delete(sessionId!);
          this.items.update((all) => ({ ...all, [sessionId!]: [] }));
          this.turns.update((all) => ({ ...all, [sessionId!]: [] }));
          this.sessionChanges.update((all) => ({ ...all, [sessionId!]: {} }));
          this.retrying.update((all) => ({ ...all, [sessionId!]: null }));
          this.hookRunning.update((all) => ({ ...all, [sessionId!]: null }));
          this.projectHooks.update((all) => ({ ...all, [sessionId!]: null }));
        }
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
          this.sessions().find((item) => item.id === sessionId)?.title ?? t("store.current_session"),
        );
        const pending = this.pendingSubmission;
        this.pendingSubmission = null;
        if (pending) {
          this.command({
            SendMessage: {
              session_id: sessionId!,
              content: pending.text,
              files: pending.files,
              images: pending.images,
              permissions: this.configuration().permissions,
              delivery: pending.delivery,
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
      case "GitStatus":
        this.gitStatus.set(data as GitStatusSnapshot);
        break;
      case "GitDiff":
        this.gitDiff.set(data as GitDiffSnapshot);
        break;
      case "PluginSnapshot": {
        const generation = data.snapshot.generation as number;
        if (generation < this.pluginGeneration) break;
        this.pluginGeneration = generation;
        const packages = data.snapshot.packages as PluginPackageInfo[];
        const issues = (data.snapshot.errors ?? []) as { error: unknown }[];
        if (issues.length > 0) {
          this.themeError.set(issues.map((issue) => errorMessage(issue.error)).join("\n"));
        }
        this.pluginPackages.set(packages);
        this.pluginConflicts.set((data.snapshot.conflicts ?? []) as PluginContributionConflict[]);
        this.pluginRuntimeContributions.set(data.snapshot.runtime_contributions ?? {});
        void this.loadPluginThemes(packages);
        break;
      }
      case "PluginActionResult": {
        const result = data.result as {
          request_id: string;
          result: { Ok?: unknown; Err?: { detail?: string } };
        };
        const pending = this.pluginConfigPending.get(result.request_id);
        if (pending) {
          this.pluginConfigPending.delete(result.request_id);
          const outcome = result.result;
          if (outcome && typeof outcome === "object" && "Err" in outcome && outcome.Err) {
            pending.reject(new Error(outcome.Err.detail ?? "plugin configuration failed"));
          } else {
            pending.resolve(outcome && typeof outcome === "object" && "Ok" in outcome ? outcome.Ok : null);
          }
        }
        break;
      }
      case "PluginSecretResult": {
        const result = data.result as { request_id: string; result: { Ok?: unknown; Err?: { detail?: string } } };
        const pending = this.pluginSecretPending.get(result.request_id);
        if (pending) {
          this.pluginSecretPending.delete(result.request_id);
          const outcome = result.result;
          if (outcome && typeof outcome === "object" && "Err" in outcome && outcome.Err) {
            pending.reject(new Error(outcome.Err.detail ?? "plugin credential update failed"));
          } else {
            pending.resolve(null);
          }
        }
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
      case "ModelInfo":
        this.modelInfo.update((all) => ({ ...all, [data.id]: data.info ?? null }));
        break;
      case "MessageRecalled":
        this.turns.update((all) => ({ ...all, [sessionId!]: (all[sessionId!] ?? []).slice(0, -1) }));
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
        this.updateItems(sessionId!, (items) => {
          const index = items.findLastIndex((item) => item.kind === "tool" && item.tool === data.tool && item.status === "running");
          if (index < 0) return items;
          const next = [...items];
          next[index] = { ...next[index], approvalPending: true } as ToolItem;
          return next;
        });
        break;
      case "ApprovalResolved":
        this.approvals.update((all) => ({
          ...all,
          [sessionId!]: (all[sessionId!] ?? []).filter((item) => item.requestId !== data.request_id),
        }));
        break;
      case "QuestionRequested":
        this.questions.update((all) => ({
          ...all,
          [sessionId!]: { requestId: data.request_id, questions: data.questions },
        }));
        break;
      case "QuestionResolved":
        if (this.questions()[sessionId!]?.requestId === data.request_id) {
          this.questions.update((all) => ({ ...all, [sessionId!]: null }));
        }
        break;
      case "PermissionsChanged":
        this.updateConfigurationFor(sessionId!, { permissions: data.permissions });
        break;
      case "PlanModeChanged":
        this.updateConfigurationFor(sessionId!, { planEnabled: data.enabled });
        break;
      case "TurnStarted": {
        const turn: TurnState = {
          id: data.turn_id,
          status: "running",
          startedAt: Date.now(),
          changes: [],
          hooks: [],
        };
        this.activeTurns.set(sessionId!, turn.id);
        this.turns.update((all) => ({ ...all, [sessionId!]: [...(all[sessionId!] ?? []), turn] }));
        this.clearRetry(sessionId!);
        this.startRate(sessionId!);
        this.setRunning(sessionId!, true);
        break;
      }
      case "TurnComplete":
        this.finishTurn(sessionId!, "done", data.duration_ms, data.stats, data.finish_reason);
        break;
      case "TurnAborted":
        this.finishTurn(sessionId!, "stopped");
        break;
      case "TurnFileChanges":
        this.updateTurn(sessionId!, { changes: data.files ?? [] });
        break;
      case "ContextUsage":
        this.contextUsage.update((all) => ({
          ...all,
          [sessionId!]: {
            used: data.used,
            total: data.total,
            cacheRead: data.cache_read_total ?? 0,
            input: data.input_total ?? 0,
          },
        }));
        break;
      case "ContextBreakdownChanged":
        this.contextBreakdowns.update((all) => ({ ...all, [sessionId!]: data.breakdown }));
        break;
      case "FileChanged":
        this.sessionChanges.update((all) => ({
          ...all,
          [sessionId!]: {
            ...(all[sessionId!] ?? {}),
            [data.path]: {
              path: data.path,
              unified_diff: data.unified_diff,
              additions: data.additions,
              deletions: data.deletions,
            },
          },
        }));
        break;
      case "FileReverted":
        this.sessionChanges.update((all) => {
          const changes = { ...(all[sessionId!] ?? {}) };
          delete changes[data.path];
          return { ...all, [sessionId!]: changes };
        });
        break;
      case "TodoListChanged":
        this.todos.update((all) => ({ ...all, [sessionId!]: data.items ?? [] }));
        break;
      case "TaskListChanged":
        this.tasks.update((all) => ({ ...all, [sessionId!]: data.tasks ?? [] }));
        break;
      case "RetryStatus":
        this.retrying.update((all) => ({ ...all, [sessionId!]: data.status }));
        break;
      case "HookStatus":
        this.hookRunning.update((all) => ({
          ...all,
          [sessionId!]: data.state === "running" ? { event: data.hook_event, label: data.label } : null,
        }));
        break;
      case "HookRunFinished":
        this.updateTurn(sessionId!, {
          hooks: [...(this.currentTurn(sessionId!)?.hooks ?? []), {
            id: `hook-${++generatedId}`,
            hook_event: data.hook_event,
            label: data.label,
            source: data.source,
            duration_ms: data.duration_ms,
            state: data.state,
            reason: data.reason,
          }],
        });
        break;
      case "ProjectHooksPending":
        this.projectHooks.update((all) => ({
          ...all,
          [sessionId!]: { path: data.path, count: data.count },
        }));
        break;
      case "ProjectHooksTrusted":
        this.projectHooks.update((all) => ({ ...all, [sessionId!]: null }));
        break;
      case "FileSearchResults":
        this.fileSearchResults.set({ query: data.query, results: data.results ?? [] });
        break;
      case "Error":
        if (sessionId) {
          const message = errorMessage(data.error);
          this.setError(sessionId, message);
          if (data.fatal !== false) {
            this.finishTurn(sessionId, "stopped");
            this.addNote(sessionId, message, true);
          }
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

  private currentTurn(sessionId: string): TurnState | undefined {
    const turns = this.turns()[sessionId] ?? [];
    const activeId = this.activeTurns.get(sessionId);
    return turns.find((turn) => turn.id === activeId) ?? turns[turns.length - 1];
  }

  private updateTurn(sessionId: string, changes: Partial<TurnState>): void {
    const activeId = this.activeTurns.get(sessionId);
    this.turns.update((all) => {
      const turns = [...(all[sessionId] ?? [])];
      const index = activeId
        ? turns.findIndex((turn) => turn.id === activeId)
        : turns.length - 1;
      if (index < 0) return all;
      turns[index] = { ...turns[index], ...changes };
      return { ...all, [sessionId]: turns };
    });
  }

  private finishTurn(
    sessionId: string,
    status: "done" | "stopped",
    durationMs?: number,
    stats?: TurnState["stats"],
    finishReason?: TurnState["finishReason"],
  ): void {
    const turn = this.currentTurn(sessionId);
    if (turn?.status === "running") {
      this.updateTurn(sessionId, {
        status,
        durationMs: durationMs ?? Math.max(0, Date.now() - turn.startedAt),
        stats,
        finishReason,
      });
    }
    this.setRunning(sessionId, false);
    this.clearRetry(sessionId);
    this.hookRunning.update((all) => ({ ...all, [sessionId]: null }));
    this.approvals.update((all) => ({ ...all, [sessionId]: [] }));
    this.questions.update((all) => ({ ...all, [sessionId]: null }));
    this.updateItems(sessionId, (items) => AgentStore.settleStreamingReasoning(items).map((item) => {
      if (item.kind === "tool" && item.approvalPending) return { ...item, approvalPending: false };
      return item;
    }));
  }

  private clearRetry(sessionId: string): void {
    if (!this.retrying()[sessionId]) return;
    this.retrying.update((all) => ({ ...all, [sessionId]: null }));
  }

  private static settleStreamingReasoning(items: ChatItem[]): ChatItem[] {
    const now = Date.now();
    let changed = false;
    const next = items.map((item) => {
      if (item.kind !== "reasoning" || !item.streaming) return item;
      changed = true;
      const replay = item.turnId?.startsWith("replay-") ?? false;
      const durationMs = !replay && item.startedAtMs !== undefined
        ? Math.max(0, now - item.startedAtMs)
        : undefined;
      return { ...item, streaming: false, durationMs };
    });
    return changed ? next : items;
  }

  private upsertConversationItems(items: ChatItem[], next: ChatItem): ChatItem[] {
    const index = items.findIndex((item) => item.id === next.id);
    if (index < 0) return [...items, next];
    const merged = [...items];
    merged[index] = this.mergeConversationItem(items[index], next);
    return merged;
  }

  private mergeConversationSnapshot(snapshot: ChatItem[], live: ChatItem[]): ChatItem[] {
    const liveById = new Map(live.map((item) => [item.id, item]));
    const snapshotIds = new Set(snapshot.map((item) => item.id));
    const merged = snapshot.map((item) => {
      const current = liveById.get(item.id);
      return current ? this.mergeConversationItem(current, item) : item;
    });
    for (const item of live) {
      if (!snapshotIds.has(item.id)) merged.push(item);
    }
    return merged;
  }

  private mergeConversationItem(previous: ChatItem, next: ChatItem): ChatItem {
    if (previous.kind === "tool" && next.kind === "tool") {
      return {
        ...previous,
        ...next,
        summary: next.summary.length >= previous.summary.length ? next.summary : previous.summary,
        output: next.output || previous.output,
        detail: next.detail || previous.detail,
        edit: next.edit ?? previous.edit,
        // A terminal row never regresses; otherwise the real transition wins
        // (an input card becomes the executing card, then done).
        status: previous.status === "done" || previous.status === "error" || previous.status === "stopped"
          ? previous.status
          : next.status,
      };
    }
    if (
      (previous.kind === "markdown" || previous.kind === "reasoning")
      && previous.kind === next.kind
    ) {
      return {
        ...previous,
        ...next,
        text: next.text.length >= previous.text.length ? next.text : previous.text,
        sources: (next.sources?.length ?? 0) > 0 ? next.sources : previous.sources,
        durationMs: next.durationMs ?? previous.durationMs,
      };
    }
    if (previous.kind === "user" && next.kind === "user") {
      return {
        ...previous,
        ...next,
        text: next.text.length >= previous.text.length ? next.text : previous.text,
        sentAtMs: next.sentAtMs || previous.sentAtMs,
      };
    }
    return { ...previous, ...next } as ChatItem;
  }

  private startRate(sessionId: string): void {
    this.rateSession = sessionId;
    this.rateStartedAt = Date.now();
    this.rateCharacters = 0;
    this.rateRenderedAt = 0;
    this.outputRate.update((all) => ({ ...all, [sessionId]: 0 }));
  }

  private addNote(sessionId: string, text: string, error: boolean, extra: Partial<NoteItem> = {}): void {
    this.updateItems(sessionId, (items) => [
      ...items,
      { kind: "note", id: `note-${++generatedId}`, text, error, ...extra } satisfies NoteItem,
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
