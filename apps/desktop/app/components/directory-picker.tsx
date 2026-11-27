import { effect, List, signal, Text, token, View } from "nsh";
import { Button, Dialog, Input } from "nsh/base";
import type { DirectoryEntry, DirectoryListing } from "../daemon-client";
import { t } from "../i18n";
import { errorMessage } from "../protocol";
import type { UiContext } from "../ui-context";
import { Icon } from "./icon";

export function DirectoryPicker(props: UiContext & {
  initialPath?: string | null;
  onSelect: (path: string) => void;
  onClose: () => void;
}) {
  const listing = signal<DirectoryListing | null>(null);
  const pathInput = signal(props.initialPath || props.store.cwd());
  const loading = signal(false);
  const error = signal("");
  let loadSeq = 0;

  const navigate = (target?: string | null) => {
    const seq = ++loadSeq;
    loading.set(true);
    error.set("");
    void props.store.listDirectory(target).then((result) => {
      if (seq !== loadSeq) return;
      listing.set(result);
      pathInput.set(result.path);
      loading.set(false);
    }, (cause) => {
      if (seq !== loadSeq) return;
      loading.set(false);
      error.set(errorMessage(cause));
    });
  };

  effect(() => {
    navigate(props.initialPath || props.store.cwd());
  });

  const currentPath = () => listing()?.path ?? pathInput().trim();
  const entries = (): DirectoryEntry[] => listing()?.entries ?? [];
  const roots = (): string[] => listing()?.roots ?? [];
  const parent = () => listing()?.parent ?? null;

  return (
    <Dialog
      open
      animation="fade"
      backdropClass="bg-popover/90"
      onOpenChange={({ open }) => { if (!open) props.onClose(); }}
      testID="directory-picker-dialog"
      class="mx-auto mt-[100px] w-[560px] max-w-[92%] overflow-hidden rounded-xl border border-border bg-popover shadow-xl"
    >
      <View class="flex-row items-center justify-between border-b border-border px-4 py-3">
        <View class="flex-row items-center gap-2">
          <Icon name="folder-open" size={16} color={token("muted-foreground")} />
          <Text class="text-sm font-medium text-foreground">{t("directory_picker.title")}</Text>
        </View>
        <Button
          testID="directory-picker-close"
          accessibilityLabel={t("common.close")}
          class="h-6 w-6 items-center justify-center rounded-(--radius) hover:bg-accent"
          onPress={props.onClose}
        >
          <Icon name="x" size={14} color={token("muted-foreground")} />
        </Button>
      </View>

      <View class="gap-2 border-b border-border px-4 py-2.5">
        <View class="flex-row items-center gap-2">
          <Button
            testID="directory-picker-up"
            disabled={!parent() || loading()}
            accessibilityLabel={t("directory_picker.up")}
            class="h-8 w-8 items-center justify-center rounded-(--radius) border border-border hover:bg-accent disabled:opacity-40"
            onPress={() => { if (parent()) navigate(parent()); }}
          >
            <Icon name="arrow-up" size={14} color={token("foreground")} />
          </Button>
          <Input
            testID="directory-picker-input"
            value={pathInput()}
            onChange={({ value }) => pathInput.set(value)}
            onSubmit={() => {
              const target = pathInput().trim();
              if (target) navigate(target);
            }}
            placeholder={t("directory_picker.path_placeholder")}
            class="h-8 min-w-0 flex-1 rounded-(--radius) border border-border bg-background px-2.5 text-xs text-foreground placeholder:text-muted-foreground"
          />
          <Button
            testID="directory-picker-go"
            disabled={loading() || !pathInput().trim()}
            class="h-8 rounded-(--radius) border border-border px-3 hover:bg-accent disabled:opacity-40"
            onPress={() => {
              const target = pathInput().trim();
              if (target) navigate(target);
            }}
          >
            <Text class="text-xs text-foreground">{t("directory_picker.go")}</Text>
          </Button>
        </View>
        <If when={roots().length > 1}>
          <View class="flex-row flex-wrap gap-1.5">
            <Each of={roots()} key={(root) => root}>
              {(root) => (
                <Button
                  class="rounded-md border border-border px-2 py-0.5 hover:bg-accent"
                  onPress={() => navigate(root())}
                >
                  <Text class="font-mono text-[11px] text-muted-foreground">{root()}</Text>
                </Button>
              )}
            </Each>
          </View>
        </If>
      </View>

      <If when={error()}>
        <Text class="border-b border-destructive/20 bg-destructive/10 px-4 py-2 text-xs text-destructive">
          {error()}
        </Text>
      </If>

      <List
        testID="directory-picker-list"
        each={entries()}
        keyExtractor={(entry) => entry.path}
        estimatedItemSize={34}
        style={{ height: 280 }}
        contentClass="p-1.5"
        empty={
          <Text class="px-3 py-8 text-center text-xs text-muted-foreground">
            {loading() ? t("directory_picker.loading") : t("directory_picker.empty")}
          </Text>
        }
      >
        {(entry) => (
          <Button
            class="h-[34px] w-full flex-row items-center gap-2 rounded-(--radius) px-2.5 hover:bg-accent"
            onPress={() => navigate(entry().path)}
          >
            <Icon name="folder-closed" size={14} color={token("muted-foreground")} />
            <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-xs text-foreground">
              {entry().name}
            </Text>
          </Button>
        )}
      </List>

      <View class="flex-row items-center justify-between gap-2 border-t border-border px-4 py-3">
        <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 font-mono text-[11px] text-muted-foreground">
          {currentPath()}
        </Text>
        <View class="flex-row items-center gap-2">
          <Button
            testID="directory-picker-cancel"
            class="rounded-(--radius) px-3 py-1.5 hover:bg-accent"
            onPress={props.onClose}
          >
            <Text class="text-xs text-foreground">{t("common.cancel")}</Text>
          </Button>
          <Button
            testID="directory-picker-confirm"
            disabled={!currentPath()}
            class="rounded-(--radius) bg-primary px-3 py-1.5 disabled:opacity-40"
            onPress={() => {
              const target = currentPath();
              if (target) props.onSelect(target);
            }}
          >
            <Text class="text-xs text-primary-foreground">{t("directory_picker.select")}</Text>
          </Button>
        </View>
      </View>
    </Dialog>
  );
}
