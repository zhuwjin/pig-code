import { Scroll, Text, View, signal, token } from "nsh";
import { Button, Input, Switch } from "nsh/base";
import { t } from "../../i18n";
import type { BotCapabilities, BotProfile } from "../../protocol";
import type { UiContext } from "../../ui-context";
import { Icon } from "../icon";

const DEFAULT_CAPABILITIES: BotCapabilities = {
  workspace_tools: true,
  control_plane: false,
  mcp: true,
  skills: true,
  network: true,
  subagents: true,
};

const CAPABILITY_KEYS: Array<{
  key: keyof BotCapabilities;
  labelKey:
    | "settings.bots.cap.workspace_tools"
    | "settings.bots.cap.control_plane"
    | "settings.bots.cap.mcp"
    | "settings.bots.cap.skills"
    | "settings.bots.cap.network"
    | "settings.bots.cap.subagents";
  descKey:
    | "settings.bots.cap.workspace_tools_desc"
    | "settings.bots.cap.control_plane_desc"
    | "settings.bots.cap.mcp_desc"
    | "settings.bots.cap.skills_desc"
    | "settings.bots.cap.network_desc"
    | "settings.bots.cap.subagents_desc";
}> = [
  {
    key: "workspace_tools",
    labelKey: "settings.bots.cap.workspace_tools",
    descKey: "settings.bots.cap.workspace_tools_desc",
  },
  {
    key: "control_plane",
    labelKey: "settings.bots.cap.control_plane",
    descKey: "settings.bots.cap.control_plane_desc",
  },
  {
    key: "mcp",
    labelKey: "settings.bots.cap.mcp",
    descKey: "settings.bots.cap.mcp_desc",
  },
  {
    key: "skills",
    labelKey: "settings.bots.cap.skills",
    descKey: "settings.bots.cap.skills_desc",
  },
  {
    key: "network",
    labelKey: "settings.bots.cap.network",
    descKey: "settings.bots.cap.network_desc",
  },
  {
    key: "subagents",
    labelKey: "settings.bots.cap.subagents",
    descKey: "settings.bots.cap.subagents_desc",
  },
];

function slugifyBotId(input: string): string {
  return input
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9_-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 48);
}

export function BotSettings(props: UiContext) {
  const { store } = props;
  const editing = signal(false);
  const editingExisting = signal(false);
  const editingBuiltin = signal(false);
  const botId = signal("");
  const name = signal("");
  const description = signal("");
  const systemPrompt = signal("");
  const capabilities = signal<BotCapabilities>({ ...DEFAULT_CAPABILITIES });
  const formError = signal("");

  const startCreate = () => {
    editing.set(true);
    editingExisting.set(false);
    editingBuiltin.set(false);
    botId.set("");
    name.set("");
    description.set("");
    systemPrompt.set("");
    capabilities.set({ ...DEFAULT_CAPABILITIES });
    formError.set("");
  };

  const startEdit = (bot: BotProfile) => {
    editing.set(true);
    editingExisting.set(true);
    editingBuiltin.set(bot.builtin);
    botId.set(bot.bot_id);
    name.set(bot.name);
    description.set(bot.description);
    systemPrompt.set(bot.system_prompt);
    capabilities.set({ ...bot.capabilities });
    formError.set("");
  };

  const cancelEdit = () => {
    editing.set(false);
    formError.set("");
  };

  const saveBot = () => {
    const trimmedName = name().trim();
    const resolvedId = editingExisting()
      ? botId().trim()
      : slugifyBotId(botId().trim() || trimmedName);
    if (!trimmedName) {
      formError.set(t("settings.bots.error_name_required"));
      return;
    }
    if (!resolvedId) {
      formError.set(t("settings.bots.error_id_required"));
      return;
    }
    const existing = store.botById(resolvedId);
    const now = Date.now();
    store.upsertBot({
      bot_id: resolvedId,
      name: trimmedName,
      avatar: existing?.avatar ?? null,
      description: description().trim(),
      system_prompt: systemPrompt().trim(),
      provider_id: existing?.provider_id ?? null,
      model_id: existing?.model_id ?? null,
      reasoning_level: existing?.reasoning_level ?? null,
      capabilities: capabilities(),
      builtin: editingBuiltin(),
      created_at_ms: existing?.created_at_ms ?? now,
      updated_at_ms: now,
    });
    editing.set(false);
    formError.set("");
  };

  const toggleCapability = (key: keyof BotCapabilities, checked: boolean) => {
    capabilities.update((prev) => ({ ...prev, [key]: checked }));
  };

  return (
    <View testID="bot-settings" class="relative min-w-0 flex-1 bg-background">
      <Scroll class="flex-1">
        <View class="w-full max-w-[960px] gap-6 p-6">
          <View class="flex-row items-start justify-between gap-4 border-b border-border pb-4">
            <View class="min-w-0 flex-1 gap-1.5">
              <Text class="text-xl font-semibold text-foreground">{t("settings.bots.title")}</Text>
              <Text class="text-sm text-muted-foreground">{t("settings.bots.description")}</Text>
            </View>
            <View class="flex-row items-center gap-2">
              <If when={Boolean(store.relayStatus()?.configured)}>
                <Button
                  testID="bots-sync"
                  disabled={store.botSyncBusy()}
                  class="flex-row items-center gap-1.5 rounded-(--radius) border border-border px-3 py-1.5 hover:bg-accent disabled:opacity-40"
                  onPress={() => void store.syncBots()}
                >
                  <Icon name="refresh-cw" size={13} color={token("foreground")} />
                  <Text class="text-sm text-foreground">{t("settings.bots.sync")}</Text>
                </Button>
              </If>
              <Button
                testID="bots-create"
                class="flex-row items-center gap-1.5 rounded-(--radius) bg-primary px-3 py-1.5 hover:opacity-90"
                onPress={startCreate}
              >
                <Icon name="plus" size={14} color={token("primary-foreground")} />
                <Text class="text-sm font-medium text-primary-foreground">{t("settings.bots.create")}</Text>
              </Button>
            </View>
          </View>

          <If when={store.botSyncNotice().length > 0}>
            <View class="rounded-xl border border-border bg-muted/40 px-4 py-2.5">
              <Text class="text-xs text-muted-foreground">{store.botSyncNotice()}</Text>
            </View>
          </If>

          <If when={editing()}>
            <View testID="bot-editor" class="gap-4 rounded-xl border border-border p-4">
              <Text class="text-base font-semibold text-foreground">
                {t(editingExisting() ? "settings.bots.edit_title" : "settings.bots.create_title")}
              </Text>
              <If when={formError().length > 0}>
                <Text testID="bot-form-error" class="text-xs text-destructive">{formError()}</Text>
              </If>
              <View class="flex-row gap-3">
                <View class="min-w-0 flex-1 gap-1.5">
                  <Text class="text-xs font-medium text-muted-foreground">{t("settings.bots.field_name")}</Text>
                  <Input
                    testID="bot-name-input"
                    value={name()}
                    placeholder={t("settings.bots.field_name_placeholder")}
                    onChange={({ value }) => name.set(value)}
                    class="rounded-(--radius) border border-border bg-background px-3 py-2 text-sm text-foreground"
                  />
                </View>
                <View class="min-w-0 flex-1 gap-1.5">
                  <Text class="text-xs font-medium text-muted-foreground">{t("settings.bots.field_id")}</Text>
                  <Input
                    testID="bot-id-input"
                    value={botId()}
                    disabled={editingExisting()}
                    placeholder={t("settings.bots.field_id_placeholder")}
                    onChange={({ value }) => botId.set(value)}
                    class="rounded-(--radius) border border-border bg-background px-3 py-2 font-mono text-sm text-foreground disabled:opacity-50"
                  />
                </View>
              </View>
              <View class="gap-1.5">
                <Text class="text-xs font-medium text-muted-foreground">{t("settings.bots.field_description")}</Text>
                <Input
                  testID="bot-description-input"
                  value={description()}
                  placeholder={t("settings.bots.field_description_placeholder")}
                  onChange={({ value }) => description.set(value)}
                  class="rounded-(--radius) border border-border bg-background px-3 py-2 text-sm text-foreground"
                />
              </View>
              <View class="gap-1.5">
                <Text class="text-xs font-medium text-muted-foreground">{t("settings.bots.field_prompt")}</Text>
                <Input
                  testID="bot-prompt-input"
                  multiline
                  value={systemPrompt()}
                  placeholder={t("settings.bots.field_prompt_placeholder")}
                  onChange={({ value }) => systemPrompt.set(value)}
                  class="min-h-24 rounded-(--radius) border border-border bg-background px-3 py-2 text-sm text-foreground"
                />
              </View>
              <View class="gap-2">
                <Text class="text-xs font-medium text-muted-foreground">{t("settings.bots.capabilities_title")}</Text>
                <View class="rounded-lg border border-border">
                  <Each of={CAPABILITY_KEYS} key={(item) => item.key}>
                    {(item, index) => (
                      <View
                        class={`flex-row items-center gap-4 p-3 ${index() < CAPABILITY_KEYS.length - 1 ? "border-b border-border" : ""}`}
                      >
                        <View class="min-w-0 flex-1 gap-0.5">
                          <Text class="text-sm font-medium text-foreground">{t(item().labelKey)}</Text>
                          <Text class="text-xs text-muted-foreground">{t(item().descKey)}</Text>
                        </View>
                        <Switch
                          testID={`bot-cap-${item().key}`}
                          checked={capabilities()[item().key]}
                          accessibilityLabel={t(item().labelKey)}
                          onChange={({ checked }) => toggleCapability(item().key, checked)}
                          trackClass="h-5 w-9 rounded-full bg-muted p-0.5 transition-colors checked:bg-primary disabled:opacity-40"
                          thumbClass="h-4 w-4 rounded-full bg-background shadow-sm transition-transform checked:translate-x-4"
                        />
                      </View>
                    )}
                  </Each>
                </View>
              </View>
              <View class="flex-row items-center gap-2 pt-1">
                <Button
                  testID="bot-save"
                  class="rounded-(--radius) bg-primary px-4 py-2 hover:opacity-90"
                  onPress={saveBot}
                >
                  <Text class="text-sm font-medium text-primary-foreground">{t("settings.bots.save")}</Text>
                </Button>
                <Button
                  testID="bot-cancel"
                  class="rounded-(--radius) border border-border px-4 py-2 hover:bg-accent"
                  onPress={cancelEdit}
                >
                  <Text class="text-sm text-foreground">{t("common.cancel")}</Text>
                </Button>
              </View>
            </View>
          </If>

          <View class="gap-3">
            <Each of={store.bots()} key={(bot) => bot.bot_id}>
              {(bot) => (
                <View
                  testID={`bot-card-${bot().bot_id}`}
                  class="gap-3 rounded-xl border border-border p-4"
                >
                  <View class="flex-row items-start justify-between gap-4">
                    <View class="min-w-0 flex-1 gap-1">
                      <View class="flex-row items-center gap-2">
                        <Icon name="bot" size={16} color={token("primary")} />
                        <Text class="text-base font-semibold text-foreground">{bot().name}</Text>
                        <Text class="font-mono text-xs text-muted-foreground">{bot().bot_id}</Text>
                        <If when={bot().builtin}>
                          <View class="rounded-full bg-muted px-2 py-0.5">
                            <Text class="text-[11px] text-muted-foreground">{t("settings.bots.builtin_badge")}</Text>
                          </View>
                        </If>
                        <If when={bot().capabilities.control_plane}>
                          <View class="rounded-full bg-primary/15 px-2 py-0.5">
                            <Text class="text-[11px] text-primary">{t("settings.bots.cap.control_plane")}</Text>
                          </View>
                        </If>
                      </View>
                      <If when={bot().description.length > 0}>
                        <Text class="text-sm text-muted-foreground">{bot().description}</Text>
                      </If>
                    </View>
                    <View class="flex-row items-center gap-2">
                      <Button
                        testID={`bot-edit-${bot().bot_id}`}
                        class="rounded-(--radius) border border-border px-3 py-1 hover:bg-accent"
                        onPress={() => startEdit(bot())}
                      >
                        <Text class="text-xs text-foreground">{t("common.edit")}</Text>
                      </Button>
                      <If when={!bot().builtin}>
                        <Button
                          testID={`bot-delete-${bot().bot_id}`}
                          class="rounded-(--radius) border border-destructive/40 px-3 py-1 hover:bg-destructive/10"
                          onPress={() => store.deleteBot(bot().bot_id)}
                        >
                          <Text class="text-xs text-destructive">{t("common.delete")}</Text>
                        </Button>
                      </If>
                    </View>
                  </View>
                  <View class="flex-row flex-wrap gap-1.5">
                    <If when={bot().capabilities.workspace_tools}>
                      <View class="rounded-full border border-border px-2 py-0.5">
                        <Text class="text-[11px] text-muted-foreground">{t("settings.bots.cap.workspace_tools")}</Text>
                      </View>
                    </If>
                    <If when={bot().capabilities.mcp}>
                      <View class="rounded-full border border-border px-2 py-0.5">
                        <Text class="text-[11px] text-muted-foreground">{t("settings.bots.cap.mcp")}</Text>
                      </View>
                    </If>
                    <If when={bot().capabilities.skills}>
                      <View class="rounded-full border border-border px-2 py-0.5">
                        <Text class="text-[11px] text-muted-foreground">{t("settings.bots.cap.skills")}</Text>
                      </View>
                    </If>
                    <If when={bot().capabilities.network}>
                      <View class="rounded-full border border-border px-2 py-0.5">
                        <Text class="text-[11px] text-muted-foreground">{t("settings.bots.cap.network")}</Text>
                      </View>
                    </If>
                    <If when={bot().capabilities.subagents}>
                      <View class="rounded-full border border-border px-2 py-0.5">
                        <Text class="text-[11px] text-muted-foreground">{t("settings.bots.cap.subagents")}</Text>
                      </View>
                    </If>
                  </View>
                </View>
              )}
            </Each>
          </View>
        </View>
      </Scroll>
    </View>
  );
}
