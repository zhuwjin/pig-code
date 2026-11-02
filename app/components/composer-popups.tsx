import { Scroll, signal, Text, token, View } from "nsh";
import { AlertDialog, Button, Checkbox, Input, Popover } from "nsh/base";
import { filterSlashCommands, modeForPermissions, permissionsForMode } from "../commands";
import type { SlashCommand } from "../commands";
import { t } from "../i18n";
import type { MessageKey } from "../i18n";
import type { ExecMode } from "../protocol";
import type { UiContext } from "../ui-context";
import { Icon, type IconName } from "./icon";
import { ProviderIcon } from "./settings/provider-icon";
import { formatTokens } from "../conversation";

const EXEC_MODES: { id: ExecMode; labelKey: MessageKey; descriptionKey: MessageKey; icon: IconName }[] = [
  { id: "read-only", labelKey: "permissions.mode.read_only", descriptionKey: "permissions.mode.read_only_description", icon: "eye" },
  { id: "auto", labelKey: "permissions.mode.auto", descriptionKey: "permissions.mode.auto_description", icon: "shield-check" },
  { id: "full", labelKey: "permissions.mode.full", descriptionKey: "permissions.mode.full_description", icon: "shield-alert" },
];

export function PermissionPicker(props: UiContext) {
  const open = signal(false);
  const configuration = () => props.store.configuration();
  const mode = () => modeForPermissions(configuration().permissions);
  const label = () => {
    const item = EXEC_MODES.find((entry) => entry.id === mode());
    return item ? t(item.labelKey) : t("permissions.mode.custom");
  };
  const select = async (value: ExecMode) => {
    if (value === "full") {
      const answer = await AlertDialog.show({
        title: t("permissions.full_access.title"),
        description: t("permissions.full_access.description"),
        actions: [
          { id: "cancel", label: t("common.cancel"), role: "cancel" },
          { id: "full", label: t("permissions.full_access.enable"), role: "destructive" },
        ],
      });
      if (answer !== "full") return;
    }
    props.store.setPermissions(permissionsForMode(value));
    open.set(false);
  };
  return (
    <View class="flex-row items-center gap-1.5">
      <Popover
        open={open()}
        onOpenChange={(value) => open.set(value)}
        placement="top"
        align="start"
        popupClass="w-80 rounded-xl border border-border bg-popover p-2 shadow-xl"
        trigger={
          <View role="button" testID="exec-mode" class="flex-row items-center gap-1 rounded-full px-3 py-1 hover:bg-accent">
            <Icon name={EXEC_MODES.find((item) => item.id === mode())?.icon ?? "shield-check"} size={16} color={token(mode() === "full" ? "destructive" : mode() === "auto" ? "warning" : "muted-foreground")} />
            <Text class={mode() === "full" ? "text-sm text-destructive" : mode() === "auto" ? "text-sm text-warning" : "text-sm text-muted-foreground"}>{label()}</Text>
            <Icon name="chevron-down" size={12} color={token("muted-foreground")} />
          </View>
        }
      >
        <View class="gap-1">
          <Each of={EXEC_MODES} key={(item) => item.id}>
            {(item) => (
              <Button
                testID={`exec-mode-${item().id}`}
                selected={mode() === item().id}
                class="rounded-(--radius) px-3 py-2 hover:bg-accent selected:bg-accent/60"
                onPress={() => void select(item().id)}
              >
                <View class="flex-row items-center gap-2">
                  <View class="min-w-0 flex-1 gap-0.5">
                    <Text class="text-sm font-medium text-foreground">{t(item().labelKey)}</Text>
                    <Text class="text-xs leading-5 text-muted-foreground">{t(item().descriptionKey)}</Text>
                  </View>
                  <If when={mode() === item().id}><Text class="text-sm text-primary">✓</Text></If>
                </View>
              </Button>
            )}
          </Each>
          <View class="my-1 h-px bg-border" />
          <Checkbox
            testID="plan-mode"
            checked={configuration().planEnabled}
            onChange={(checked) => props.store.setPlanMode(checked)}
            class="flex-row items-center gap-3 rounded-(--radius) px-3 py-2 hover:bg-accent"
            indicatorClass="h-4 w-4 rounded border border-border"
          >
            <View class="flex-1 gap-0.5">
              <Text class="text-sm font-medium text-foreground">{t("composer.plan_mode")}</Text>
              <Text class="text-xs leading-5 text-muted-foreground">{t("composer.plan_mode_description")}</Text>
            </View>
          </Checkbox>
        </View>
      </Popover>
      <If when={configuration().planEnabled}>
        <Button
          testID="plan-chip"
          accessibilityLabel={t("composer.close_plan_mode")}
          class="group flex-row items-center gap-1 rounded-full bg-accent/50 px-3 py-1 hover:bg-accent"
          onPress={() => props.store.setPlanMode(false)}
        >
          <Icon name="lightbulb" size={15} class="group-hover:hidden" color={token("info")} />
          <Icon name="x" size={14} class="hidden group-hover:block" color={token("muted-foreground")} />
          <Text class="text-sm text-info">Plan</Text>
        </Button>
      </If>
    </View>
  );
}

export function ModelPicker(props: UiContext) {
  const open = signal(false);
  const query = signal("");
  const triggerWidth = signal(0);
  const models = () => {
    const normalized = query().trim().toLowerCase();
    return props.store.modelOptions().filter(({ provider, model }) =>
      !normalized || `${provider.name} ${model.name ?? model.id} ${model.id}`.toLowerCase().includes(normalized)
    );
  };
  const providers = () => (props.store.config()?.providers ?? [])
    .filter((provider) => provider.enabled)
    .map((provider) => ({ provider, models: models().filter((option) => option.provider.id === provider.id) }))
    .filter((entry) => entry.models.length > 0);
  const selected = () => props.store.selectedModel();
  const reasoning = () => props.store.configuration().reasoningLevel;
  const reasoningLabel = () => props.store.reasoningOptions().find((option) => option.id === reasoning())?.label;
  return (
    <Popover
      open={open()}
      onOpenChange={(value) => {
        open.set(value);
        if (!value) query.set("");
      }}
      placement="top"
      align="end"
      popupClass="w-96 rounded-xl border border-border bg-popover p-2 shadow-xl"
      trigger={
        <View
          role="button"
          testID="model-picker"
          class="max-w-56 flex-row items-center gap-1 rounded-full px-3 py-1 hover:bg-accent"
          style={{ width: open() && triggerWidth() > 0 ? triggerWidth() : undefined }}
          onLayout={(bounds) => { if (!open()) triggerWidth.set(bounds.size.width); }}
        >
          <Text numberOfLines={1} class="text-sm text-foreground">
            {open() ? t("composer.select_model") : (props.store.currentModel() || t("composer.select_model"))}
          </Text>
          <If when={!open() && props.store.currentModel() && reasoningLabel()}>
            <Text numberOfLines={1} class="text-sm text-muted-foreground">{reasoningLabel()}</Text>
          </If>
          <Icon name="chevron-down" size={12} color={token("muted-foreground")} />
        </View>
      }
    >
      <View class="gap-2">
        <Input
          testID="model-search"
          value={query()}
          onChange={(text) => query.set(text)}
          placeholder={t("composer.search_models")}
          class="rounded-(--radius) border border-border bg-background px-3 py-2 selection:bg-foreground text-sm text-foreground placeholder:text-muted-foreground"
        />
        <Scroll class="max-h-72">
          <View class="gap-1">
            <Each of={providers()} key={(entry) => entry.provider.id}>
              {(entry) => (
                <View class="gap-1">
                  <View class="flex-row items-center gap-2 px-3 pb-1 pt-2">
                    <ProviderIcon icon={entry().provider.icon} size={16} />
                    <Text class="text-xs font-medium text-muted-foreground">{entry().provider.name}</Text>
                  </View>
                  <Each of={entry().models} key={(option) => `${option.provider.id}/${option.model.id}`}>
                    {(option) => {
                      const value = () => option();
                      const active = () => selected()?.provider.id === value().provider.id && selected()?.model.id === value().model.id;
                      return (
                        <Button
                          testID={`model-option-${value().provider.id}-${value().model.id}`}
                          selected={active()}
                          class="rounded-(--radius) px-3 py-2 hover:bg-accent selected:bg-accent/60"
                          onPress={() => {
                            props.store.selectModel(value().provider.id, value().model.id);
                            open.set(false);
                          }}
                        >
                          <View class="flex-row items-center gap-2">
                            <ProviderIcon icon={value().model.icon} kind="model" size={20} />
                            <View class="min-w-0 flex-1 gap-0.5">
                              <Text numberOfLines={1} class="text-sm font-medium text-foreground">{value().model.name || value().model.id}</Text>
                              <Text numberOfLines={1} class="text-xs text-muted-foreground">
                                {value().model.id} · {formatTokens(value().model.context_window)}
                              </Text>
                            </View>
                            <If when={value().model.input_image}>
                              <Text class="rounded-full bg-accent px-1.5 py-0.5 text-[10px] text-muted-foreground">{t("settings.models.vision")}</Text>
                            </If>
                            <If when={active()}><Text class="text-sm text-primary">✓</Text></If>
                          </View>
                        </Button>
                      );
                    }}
                  </Each>
                </View>
              )}
            </Each>
            <If when={models().length === 0}>
              <Text class="px-3 py-4 text-center text-xs text-muted-foreground">{t("composer.no_models")}</Text>
            </If>
          </View>
        </Scroll>
        <If when={props.store.reasoningOptions().length > 0}>
          <View class="gap-1 border-t border-border pt-2">
            <Text class="px-2 text-xs font-medium text-muted-foreground">{t("composer.reasoning_level")}</Text>
            <View class="flex-row flex-wrap gap-1">
              <Button
                testID="reasoning-off"
                selected={!reasoning()}
                class="rounded-full px-3 py-1 hover:bg-accent selected:bg-accent"
                onPress={() => props.store.setReasoning(null)}
              >
                <Text class="text-xs text-foreground">{t("composer.reasoning_off")}</Text>
              </Button>
              <Each of={props.store.reasoningOptions()} key={(option) => option.id}>
                {(option) => (
                  <Button
                    testID={`reasoning-${option().id}`}
                    selected={reasoning() === option().id}
                    class="rounded-full px-3 py-1 hover:bg-accent selected:bg-accent"
                    onPress={() => props.store.setReasoning(option().id)}
                  >
                    <Text class="text-xs text-foreground">{option().label}</Text>
                  </Button>
                )}
              </Each>
            </View>
          </View>
        </If>
        <Button
          testID="manage-models"
          class="flex-row items-center gap-2 rounded-(--radius) border-t border-border px-3 py-2 hover:bg-accent"
          onPress={() => {
            open.set(false);
            props.store.pluginNavigation.set("models");
          }}
        >
          <Icon name="settings" size={14} color={token("muted-foreground")} />
          <Text class="text-xs text-muted-foreground">{t("composer.manage_models")}</Text>
        </Button>
      </View>
    </Popover>
  );
}

export function SlashCandidates(props: {
  query: string;
  selected: number;
  onSelect: (command: SlashCommand["name"]) => void;
}) {
  const commands = () => filterSlashCommands(props.query);
  return (
    <View testID="slash-candidates" class="absolute bottom-full left-0 right-0 z-40 mb-2 gap-1 rounded-xl border border-border bg-popover p-2 shadow-xl">
      <Each of={commands()} key={(command) => command.name}>
        {(command, index) => (
          <Button
            testID={`slash-${command().name}`}
            selected={index() === props.selected}
            class="flex-row items-center gap-3 rounded-(--radius) px-3 py-2 hover:bg-accent selected:bg-accent"
            onPress={() => props.onSelect(command().name)}
          >
            <Text class="w-20 font-mono text-xs text-foreground">{command().name}</Text>
            <Text class="min-w-0 flex-1 text-xs text-muted-foreground">{t(command().descriptionKey)}</Text>
          </Button>
        )}
      </Each>
    </View>
  );
}
