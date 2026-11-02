import { effect, Image, keymap, signal, Text, token, View, type YNode } from "nsh";
import { Button, Dialog, Input, Popover } from "nsh/base";
import type { SlashCommand } from "../commands";
import { filterSlashCommands } from "../commands";
import { fileComposerImage } from "../media";
import type { UiContext } from "../ui-context";
import { workspaceName } from "../ui-context";
import { ApprovalCard } from "./approval-card";
import { ComposerAux } from "./composer-aux";
import { ModelPicker, PermissionPicker, SlashCandidates } from "./composer-popups";
import { Icon } from "./icon";
import { hostPatchProps, UiNodeFactory, UiView } from "../plugins/ui-node-factory";
import { QuestionCard } from "./question-card";
import { QueuePanel } from "./queue-panel";

function WorkspaceSelector(props: UiContext) {
  const open = signal(false);
  const selected = () => props.store.heroCwd();
  const workspaces = () => Array.from(new Set([
    props.store.cwd(),
    ...props.store.sessions().map((session) => session.cwd),
    ...(selected() ? [selected()!] : []),
  ]));
  return (
    <View class="w-full flex-row items-center gap-2 border-b border-border pb-1">
      <Popover
        open={open()}
        onOpenChange={(value) => open.set(value)}
        placement="top"
        align="start"
        popupClass="w-80 rounded-xl border border-border bg-popover p-2 shadow-xl"
        trigger={
          <View role="button" testID="hero-cwd" class="flex-row items-center gap-1 rounded-full bg-accent/50 px-3 py-1 hover:bg-accent">
            <Icon name="folder-closed" color={token("muted-foreground")} />
            <Text class={selected() ? "text-sm text-foreground" : "text-sm text-muted-foreground"}>
              {selected() ? workspaceName(selected()!) : "选择工作区"}
            </Text>
            <Icon name="chevron-down" size={12} color={token("muted-foreground")} />
          </View>
        }
      >
        <View class="gap-1">
          <Each of={workspaces()} key={(path) => path}>
            {(path, index) => (
              <Button
                testID={`hero-cwd-option-${index()}`}
                selected={selected() === path()}
                class="rounded-(--radius) px-3 py-2 hover:bg-accent selected:bg-accent/60"
                onPress={() => {
                  props.store.selectHeroWorkspace(path());
                  open.set(false);
                }}
              >
                <View class="min-w-0 gap-0.5">
                  <Text numberOfLines={1} class="text-sm text-foreground">{workspaceName(path())}</Text>
                  <Text numberOfLines={1} class="text-xs text-muted-foreground">{path()}</Text>
                </View>
              </Button>
            )}
          </Each>
          <If when={selected()}>
            <Button
              testID="hero-cwd-clear"
              class="rounded-(--radius) px-3 py-2 hover:bg-accent"
              onPress={() => {
                props.store.selectHeroWorkspace(null);
                open.set(false);
              }}
            >
              <Text class="text-sm text-muted-foreground">清除工作区</Text>
            </Button>
          </If>
        </View>
      </Popover>
    </View>
  );
}

function AttachmentStrip(props: UiContext & { onPreview: (source: string) => void }) {
  const files = () => props.store.draftFiles();
  const images = () => props.store.draftImages();
  return (
    <If when={files().length > 0 || images().length > 0}>
      <View class="w-full flex-row flex-wrap gap-2">
        <Each of={files()} key={(path) => path}>
          {(path) => (
            <View class="max-w-56 flex-row items-center gap-1 rounded-lg bg-accent px-2 py-1">
              <Icon name="file-text" size={13} color={token("muted-foreground")} />
              <Text numberOfLines={1} class="min-w-0 text-xs text-foreground">{path().split(/[\\/]/).pop()}</Text>
              <Button accessibilityLabel={`移除 ${path()}`} onPress={() => props.store.removeDraftFile(path())}>
                <Icon name="x" size={11} color={token("muted-foreground")} />
              </Button>
            </View>
          )}
        </Each>
        <Each of={images()} key={(image) => image.id}>
          {(image) => (
            <View class="relative h-16 w-20">
              <Button
                testID={`composer-image-${image().id}`}
                class="h-16 w-20 overflow-hidden rounded-lg border border-border"
                onPress={() => props.onPreview(image().preview)}
              >
                <Image src={image().preview} resizeMode="cover" class="h-16 w-20" />
              </Button>
              <Button
                accessibilityLabel="移除图片"
                class="absolute -right-1 -top-1 h-5 w-5 items-center justify-center rounded-full border border-border bg-background"
                onPress={() => props.store.removeDraftImage(image().id)}
              >
                <Icon name="x" size={10} color={token("muted-foreground")} />
              </Button>
            </View>
          )}
        </Each>
      </View>
    </If>
  );
}

function MentionCandidates(props: {
  paths: string[];
  selected: number;
  onSelect: (path: string) => void;
}) {
  return (
    <View testID="mention-candidates" class="absolute bottom-full left-0 right-0 z-40 mb-2 max-h-64 gap-1 overflow-hidden rounded-xl border border-border bg-popover p-2 shadow-xl">
      <Each of={props.paths} key={(path) => path}>
        {(path, index) => (
          <Button
            testID={`mention-${index()}`}
            selected={index() === props.selected}
            class="flex-row items-center gap-2 rounded-(--radius) px-3 py-2 hover:bg-accent selected:bg-accent"
            onPress={() => props.onSelect(path())}
          >
            <Icon name="file-text" size={14} color={token("muted-foreground")} />
            <Text numberOfLines={1} class="min-w-0 flex-1 text-xs text-foreground">{path()}</Text>
          </Button>
        )}
      </Each>
      <If when={props.paths.length === 0}>
        <Text class="px-3 py-3 text-xs text-muted-foreground">没有匹配文件</Text>
      </If>
    </View>
  );
}

export function Composer(props: UiContext & { hero?: boolean }) {
  const { store, themeStore } = props;
  const slashIndex = signal(0);
  const mentionIndex = signal(0);
  const inputFocused = signal(false);
  const caret = signal(0);
  const preview = signal<string | null>(null);
  let inputNode: YNode | undefined;
  const sessionId = () => store.current();
  const running = () => {
    const id = sessionId();
    return id ? (store.running()[id] ?? false) : false;
  };
  const approval = () => {
    const id = sessionId();
    return id ? (store.approvals()[id] ?? [])[0] : undefined;
  };
  const question = () => {
    const id = sessionId();
    return id ? store.questions()[id] : null;
  };
  const slashQuery = () => /^\/([^\s]*)$/.exec(store.draft())?.[1] ?? null;
  const slashCommands = () => {
    const query = slashQuery();
    return query === null || approval() || question() ? [] : filterSlashCommands(query);
  };
  const mention = () => {
    if (approval() || question()) return null;
    const before = store.draft().slice(0, Math.min(caret(), store.draft().length));
    return /(?:^|\s)@([^\s@]*)$/.exec(before);
  };
  const mentionPaths = () => {
    const current = mention();
    if (!current) return [];
    const search = store.fileSearchResults();
    return search.query === current[1] ? search.results : [];
  };
  const chooseSlash = (name: SlashCommand["name"]) => {
    store.draft.set(`${name} `);
    caret.set(name.length + 1);
    inputNode?.setSelection(name.length + 1);
  };
  const chooseMention = (path: string) => {
    const current = mention();
    if (!current) return;
    const start = (current.index ?? 0) + current[0].lastIndexOf("@");
    const value = `${store.draft().slice(0, start)}@${path} ${store.draft().slice(caret())}`;
    store.draft.set(value);
    store.addDraftFile(path);
    const position = start + path.length + 2;
    caret.set(position);
    inputNode?.setSelection(position);
    store.fileSearchResults.set({ query: "", results: [] });
  };
  const submit = (opposite = false) => {
    if (approval() || question()) return;
    if (!opposite && mention()) {
      const paths = mentionPaths();
      if (paths.length > 0) {
        chooseMention(paths[Math.min(mentionIndex(), paths.length - 1)]);
        return;
      }
    }
    const commands = slashCommands();
    if (!opposite && commands.length > 0) {
      chooseSlash(commands[Math.min(slashIndex(), commands.length - 1)].name);
      return;
    }
    if (opposite) store.sendWithOppositeDelivery();
    else store.send();
  };
  const insertDraft = (text: string) => {
    if (!text) return;
    const position = Math.min(caret(), store.draft().length);
    const value = store.draft().slice(0, position) + text + store.draft().slice(position);
    store.draft.set(value);
    caret.set(position + text.length);
    inputNode?.setSelection(position + text.length);
  };
  effect(() => {
    store.draft();
    slashIndex.set(0);
    mentionIndex.set(0);
  });
  effect(() => {
    const current = mention();
    if (!current) return;
    const timer = setTimeout(() => store.searchFiles(current[1]), 120);
    return () => clearTimeout(timer);
  });
  effect(() => {
    if (!inputFocused()) return;
    return keymap.bind({
      enter: () => submit(),
      "ctrl+enter": () => submit(true),
      "cmd+enter": () => submit(true),
    }, { when: "always" });
  });
  effect(() => {
    if (!inputFocused()) return;
    const count = mention() ? mentionPaths().length : slashCommands().length;
    if (count === 0) return;
    return keymap.bind({
      up: () => {
        if (mention()) mentionIndex.update((index) => (index - 1 + count) % count);
        else slashIndex.update((index) => (index - 1 + count) % count);
      },
      down: () => {
        if (mention()) mentionIndex.update((index) => (index + 1) % count);
        else slashIndex.update((index) => (index + 1) % count);
      },
      escape: () => {
        if (mention()) {
          const current = mention()!;
          const start = (current.index ?? 0) + current[0].lastIndexOf("@");
          store.draft.set(store.draft().slice(0, start) + store.draft().slice(caret()));
        } else if (/^\/[^\s]*$/.test(store.draft())) store.draft.set("");
      },
    }, { when: "always" });
  });
  const queue = () => store.currentQueue();
  const showQueue = () => !props.hero && !approval() && !question() && (
    queue().pending.length > 0 || queue().steer.length > 0 || queue().paused || queue().held
  );
  const sendWithContributions = () => (
    <UiNodeFactory
      uiId="composer.sendButton"
      store={store}
      render={(patch, children) => (
        <Button
          testID="send-button"
          accessibilityLabel={(patch.accessibility.accessibilityLabel as string | undefined) ?? "发送"}
          disabled={patch.props.disabled === undefined
            ? !store.draft().trim() && store.draftImages().length === 0
            : Boolean(patch.props.disabled)}
          class="h-6 w-6 items-center justify-center rounded-full bg-primary active:bg-primary/90 disabled:opacity-30"
          style={patch.style}
          {...hostPatchProps(patch, { onPress: () => store.send() })}
        >
          {children ?? <Icon name="arrow-up" size={14} color={token("primary-foreground")} />}
        </Button>
      )}
    />
  );
  return (
    <View class={props.hero ? "p-3 pb-1" : "px-4 pb-1 pt-2"}>
      <View class="mx-auto w-full" style={{ maxWidth: 860 }}>
        <If when={showQueue()}><QueuePanel {...props} /></If>
        <UiView
          uiId="composer.root"
          store={store}
          class={themeStore.classes(
            "composer.root",
            inputFocused()
              ? "gap-2 rounded-2xl border border-ring/40 bg-composer px-3 pb-2 pt-2"
              : "gap-2 rounded-2xl border border-input bg-composer px-3 pb-2 pt-2",
          )}
        >
          <If when={props.hero}><WorkspaceSelector {...props} /></If>
          <If when={!props.hero && (store.currentTodos().length > 0 || store.currentTasks().length > 0 || store.currentChanges().length > 0)}>
            <ComposerAux {...props} />
          </If>
          <If when={question()}>
            {(value) => <QuestionCard {...props} question={value()} />}
          </If>
          <If when={approval()}>
            {(value) => <ApprovalCard {...props} approval={value()} />}
          </If>
          <If when={!question() && !approval()}>
            <AttachmentStrip {...props} onPreview={(source) => preview.set(source)} />
            <View class="relative">
              <If when={slashCommands().length > 0}>
                <SlashCandidates query={slashQuery() ?? ""} selected={slashIndex()} onSelect={chooseSlash} />
              </If>
              <If when={mention()}>
                <MentionCandidates paths={mentionPaths()} selected={mentionIndex()} onSelect={chooseMention} />
              </If>
              <UiNodeFactory
                uiId="composer.input"
                store={store}
                captureFocus={() => inputFocused()}
                restoreFocus={() => setTimeout(() => inputNode?.focus(), 0)}
                render={(patch) => (
                  <Input
                    ref={(node) => inputNode = node}
                    testID="composer-input"
                    value={store.draft()}
                    {...hostPatchProps(patch, {
                      onChange: (text: string) => store.draft.set(text),
                      onSubmit: () => submit(),
                      onFocus: () => inputFocused.set(true),
                      onBlur: () => inputFocused.set(false),
                      onSelectionChange: (selection: { start: number }) => caret.set(selection.start),
                      onPaste: (event: { text: string; files: File[]; uris: string[] }) => {
                        void (async () => {
                          insertDraft(event.text);
                          const remaining = Math.max(0, 8 - store.draftImages().length);
                          const images = event.files.filter((file) => file.type.startsWith("image/")).slice(0, remaining);
                          const converted = await Promise.all(images.map(fileComposerImage));
                          store.addDraftImages(converted);
                          if (images.length < event.files.filter((file) => file.type.startsWith("image/")).length) {
                            store.globalError.set("每条消息最多添加 8 张图片");
                          }
                          for (const uri of event.uris) {
                            const path = decodeURIComponent(uri.replace(/^file:\/\//, "").replace(/^\/(?=[A-Za-z]:)/, ""));
                            if (path) store.addDraftFile(path);
                          }
                        })();
                      },
                    })}
                    multiline={patch.props.multiline === undefined ? true : Boolean(patch.props.multiline)}
                    disabled={patch.props.disabled as boolean | undefined}
                    pasteBehavior="report"
                    submitBehavior="submit"
                    placeholder={(patch.props.placeholder as string | undefined) ?? (running()
                      ? store.config()?.default_delivery === "steer"
                        ? "继续输入以引导后续修改"
                        : "继续输入以排队后续修改"
                      : store.config()?.default_delivery === "steer"
                        ? "按 Enter 发送引导消息 · 按 Ctrl+Enter 加入队列"
                        : "按 Enter 加入队列 · 按 Ctrl+Enter 发送引导消息")}
                    class={themeStore.classes(
                      "composer.input",
                      "max-h-40 min-h-10 px-1 text-foreground selection:bg-foreground placeholder:text-muted-foreground",
                    )}
                    style={[{ fontSize: 14 }, patch.style]}
                    {...patch.accessibility}
                  />
                )}
              />
            </View>
            <UiView uiId="composer.actions" store={store} class="flex-row items-center gap-2">
              <PermissionPicker {...props} />
              <View class="flex-1" />
              <ModelPicker {...props} />
              <If when={running()}>
                <Button
                  testID="stop-button"
                  accessibilityLabel="停止"
                  class="h-6 w-6 items-center justify-center rounded-full bg-destructive"
                  onPress={() => store.stop()}
                >
                  <Icon name="square" size={10} color="#ffffff" />
                </Button>
              </If>
              {sendWithContributions()}
            </UiView>
          </If>
        </UiView>
      </View>
      <Dialog
        open={Boolean(preview())}
        animation="fade"
        backdropClass="bg-black/85"
        onOpenChange={(open) => { if (!open) preview.set(null); }}
      >
        <View class="relative h-[720px] w-[1000px] max-w-[92%] items-center justify-center">
          <If when={preview()}>
            {(source) => <Image src={source()} resizeMode="contain" class="h-full w-full" />}
          </If>
          <Button
            accessibilityLabel="关闭图片"
            class="absolute right-3 top-3 h-8 w-8 items-center justify-center rounded-full bg-black/50"
            onPress={() => preview.set(null)}
          >
            <Icon name="x" size={16} color="#ffffff" />
          </Button>
        </View>
      </Dialog>
    </View>
  );
}
