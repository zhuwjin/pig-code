import { batch, Clipboard, effect, Image, keymap, signal, Text, token, View } from "nsh";
import { Button, Command, type CommandItem, focus as focusEditor, Popover, RichEditor } from "nsh/base";
import type { SlashCommand } from "../commands";
import { filterSlashCommands } from "../commands";
import {
  buildComposerDocument,
  createComposerEditor,
  insertComposerMention,
  measureComposerEditorHeight,
  measureComposerEditorLineHeight,
  readComposerDocument,
} from "../composer-editor";
import { t } from "../i18n";
import { composerImage } from "../media";
import { errorMessage } from "../protocol";
import type { UiContext } from "../ui-context";
import { workspaceName } from "../ui-context";
import { ApprovalCard } from "./approval-card";
import { ComposerAux } from "./composer-aux";
import { ModelPicker, PermissionPicker, SlashCandidates } from "./composer-popups";
import { DirectoryPicker } from "./directory-picker";
import { Icon } from "./icon";
import { ImageViewer } from "./image-viewer";
import { QuestionCard } from "./question-card";
import { QueuePanel } from "./queue-panel";
import { SessionStatusBar } from "./stats";

function WorkspaceSelector(props: UiContext) {
  const open = signal(false);
  const pickerOpen = signal(false);
  const query = signal("");
  const selected = () => props.store.heroCwd();
  const workspaces = () => Array.from(new Set([
    props.store.cwd(),
    ...props.store.sessions().map((session) => session.cwd),
    ...(selected() ? [selected()!] : []),
  ]));
  const items = (): CommandItem[] => [
    ...workspaces().map((path) => ({
      id: `workspace:${path}`,
      label: workspaceName(path),
      keywords: [path],
      checked: selected() === path,
      group: "workspaces",
    })),
    { id: "open-folder", label: `${t("composer.open_folder")} …`, group: "actions" },
    ...(selected() ? [{ id: "clear-workspace", label: t("composer.no_workspace"), group: "actions" }] : []),
  ];
  return (
    <View class="w-full flex-row items-center gap-2 border-b border-border pb-1">
      <Popover
        open={open()}
        onOpenChange={({ open: value }) => {
          open.set(value);
          query.set("");
        }}
        closeOnEscape={false}
        onCancel={() => {
          if (query()) query.set("");
          else open.set(false);
        }}
        placement="top"
        align="start"
        offset={8}
        margin={8}
        popupClass="w-64 overflow-hidden rounded-xl border border-border bg-popover shadow-xl"
        trigger={
          <View role="button" testID="hero-cwd" class="flex-row items-center gap-1 rounded-full bg-accent/50 px-3 py-1 hover:bg-accent">
            <Icon name="folder-closed" color={token("muted-foreground")} />
            <Text class={selected() ? "text-sm text-foreground" : "text-sm text-muted-foreground"}>
              {selected() ? workspaceName(selected()!) : t("composer.select_workspace")}
            </Text>
            <Icon name="chevron-down" size={12} color={token("muted-foreground")} />
          </View>
        }
      >
        <Command
          testID="hero-cwd-command"
          items={items()}
          query={query()}
          onQueryChange={({ query: value }) => query.set(value)}
          autoFocus
          placeholder={t("composer.search_workspace")}
          accessibilityLabel={t("composer.select_workspace")}
          searchAccessibilityLabel={t("composer.search_workspace")}
          class="w-64 bg-popover"
          searchBarClass="h-10 flex-row items-center gap-2 border-b border-border px-3"
          searchClass="min-w-0 flex-1 text-sm text-foreground placeholder:text-muted-foreground"
          searchPrefix={<Icon name="search" size={14} color={token("muted-foreground")} />}
          listClass="max-h-64"
          itemClass="group flex-row items-center gap-2 px-3 py-2 focus:bg-accent checked:bg-accent/60"
          groupClass="hidden"
          separatorClass="h-px bg-border"
          empty={<Text class="px-3 py-2 text-sm text-muted-foreground">{t("composer.no_matching_workspaces")}</Text>}
          renderItem={(entry) => {
            const isWorkspace = entry.id.startsWith("workspace:");
            const testID = isWorkspace
              ? `hero-cwd-option-${workspaces().indexOf(entry.id.slice("workspace:".length))}`
              : entry.id === "clear-workspace" ? "hero-cwd-clear" : `hero-cwd-${entry.id}`;
            return (
              <View testID={testID} class="min-w-0 flex-1 flex-row items-center gap-2">
                <Icon
                  name={entry.id === "open-folder" ? "folder-open" : entry.id === "clear-workspace" ? "circle-x" : "folder-closed"}
                  size={14}
                  color={token("muted-foreground")}
                />
                <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-sm text-foreground">{entry.label}</Text>
                <If when={entry.checked}><Icon name="check" size={14} color={token("foreground")} /></If>
              </View>
            );
          }}
          onCancel={() => open.set(false)}
          onSelect={({ id }) => {
            if (id.startsWith("workspace:")) {
              props.store.selectHeroWorkspace(id.slice("workspace:".length));
              open.set(false);
              return;
            }
            open.set(false);
            if (id === "clear-workspace") {
              props.store.selectHeroWorkspace(null);
              return;
            }
            pickerOpen.set(true);
          }}
        />
      </Popover>
      <If when={pickerOpen()}>
        <DirectoryPicker
          {...props}
          initialPath={selected() ?? props.store.cwd()}
          onSelect={(path) => {
            props.store.selectHeroWorkspace(path);
            pickerOpen.set(false);
          }}
          onClose={() => pickerOpen.set(false)}
        />
      </If>
    </View>
  );
}

function AttachmentStrip(props: UiContext & {
  onPreview: (index: number) => void;
  onMention: (index: number) => void;
}) {
  const images = () => props.store.draftImages();
  return (
    <If when={images().length > 0}>
      <View class="w-full flex-row flex-wrap gap-2">
        <Each of={images()} key={(image) => image.id}>
          {(image, index) => (
            <View class="group relative h-14 w-14">
              <Button
                testID={`composer-image-${image().id}`}
                accessibilityLabel={t("messages.image_label", { index: index() + 1 })}
                class="h-14 w-14 overflow-hidden rounded-lg border border-border"
                onPress={() => props.onPreview(index())}
              >
                <Image src={image().preview} style={{ objectFit: "cover" }} class="h-14 w-14" />
              </Button>
              <View
                class="absolute inset-0 rounded-lg bg-black/45 opacity-0 group-hover:opacity-100"
                style={{ pointerEvents: "box-none" }}
              >
                <View class="absolute inset-0 flex-row items-center justify-center gap-1" style={{ pointerEvents: "box-none" }}>
                  <Button
                    testID={`composer-image-mention-${image().id}`}
                    accessibilityLabel={t("composer.image_mention", { index: index() + 1 })}
                    class="h-5 w-5 items-center justify-center rounded-full border border-border bg-background/90 text-foreground hover:bg-accent"
                    onPress={() => props.onMention(index() + 1)}
                  >
                    <Icon name="at-sign" size={12} color={token("foreground")} />
                  </Button>
                  <Button
                    testID={`composer-image-remove-${image().id}`}
                    accessibilityLabel={t("composer.remove_image")}
                    class="h-5 w-5 items-center justify-center rounded-full border border-border bg-background/90 text-foreground hover:bg-accent"
                    onPress={() => props.store.removeDraftImage(image().id)}
                  >
                    <Icon name="trash" size={12} color={token("foreground")} />
                  </Button>
                </View>
              </View>
              <View
                testID={`composer-image-badge-${image().id}`}
                class="absolute -bottom-1 -right-1 h-5 min-w-5 items-center justify-center rounded-full border border-border bg-background px-1"
                style={{ pointerEvents: "none" }}
              >
                <Text style={{ fontSize: 10, lineHeight: 12 }} class="text-foreground">{index() + 1}</Text>
              </View>
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
            <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-xs text-foreground">{path()}</Text>
          </Button>
        )}
      </Each>
      <If when={props.paths.length === 0}>
        <Text class="px-3 py-3 text-xs text-muted-foreground">{t("composer.no_matching_files")}</Text>
      </If>
    </View>
  );
}

export function Composer(props: UiContext & { hero?: boolean; onHeightChange?: (height: number) => void }) {
  const { store, themeStore } = props;
  const slashIndex = signal(0);
  const mentionIndex = signal(0);
  const inputFocused = signal(false);
  const editorRevision = signal(0);
  const preview = signal<number | null>(null);
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
  const slash = () => {
    editorRevision();
    if (approval() || question()) return null;
    const state = editor.suggestion("slash");
    return state?.active ? state : null;
  };
  const slashQuery = () => slash()?.query ?? null;
  const slashCommands = () => {
    const query = slashQuery();
    return query === null ? [] : filterSlashCommands(query);
  };
  const mention = () => {
    editorRevision();
    if (approval() || question()) return null;
    const state = editor.suggestion("mention");
    return state?.active ? state : null;
  };
  const mentionPaths = () => {
    const current = mention();
    if (!current) return [];
    const search = store.fileSearchResults();
    return search.query === (current.query ?? "") ? search.results : [];
  };
  const chooseSlash = (name: SlashCommand["name"]) => {
    const current = slash();
    if (!current) return;
    const label = name === "/compact"
      ? t("commands.compact_label")
      : name === "/plan"
        ? t("commands.plan_label")
        : t("commands.clear_label");
    insertComposerMention(editor, "command", name, label, current.range);
    editor.focus();
  };
  const chooseMention = (path: string) => {
    const current = mention();
    if (!current) return;
    insertComposerMention(editor, "file", path, path.split(/[\\/]/).pop() ?? path, current.range);
    editor.focus();
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
  const editor = createComposerEditor(submit, () => store.threadSearchOpen.set(true));
  const editorHeight = signal(40);
  let editorResizeFrame: number | undefined;
  let editorWidth = 0;
  let editorLineHeight = 0;
  const syncDraft = () => {
    const value = readComposerDocument(editor.doc, store.draftImages());
    const files = store.draftFiles();
    batch(() => {
      if (store.draft() !== value.text) store.draft.set(value.text);
      if (value.files.length !== files.length || value.files.some((file, index) => file !== files[index])) {
        store.draftFiles.set(value.files);
      }
      editorRevision.set(editor.revision);
    });
  };
  const scheduleEditorResize = () => {
    if (editorResizeFrame !== undefined) cancelAnimationFrame(editorResizeFrame);
    editorResizeFrame = requestAnimationFrame(() => {
      editorResizeFrame = undefined;
      const measured = measureComposerEditorHeight(editor);
      const lineHeight = measureComposerEditorLineHeight(editor);
      if (lineHeight !== null) editorLineHeight = lineHeight;
      const maxHeight = editorLineHeight > 0 ? 8 + editorLineHeight * 10 : 208;
      if (measured !== null) editorHeight.set(Math.max(40, Math.min(maxHeight, measured)));
    });
  };
  editor.on("revision", () => {
    syncDraft();
    scheduleEditorResize();
  });
  effect(() => () => {
    if (editorResizeFrame !== undefined) cancelAnimationFrame(editorResizeFrame);
  });
  effect(() => {
    editorRevision();
    const text = store.draft();
    const files = store.draftFiles();
    const images = store.draftImages();
    const current = readComposerDocument(editor.doc, images);
    const sameFiles = current.files.length === files.length && current.files.every((file, index) => file === files[index]);
    if (current.current && current.text === text && sameFiles) return;
    editor.setContent(buildComposerDocument(text, files, images));
    editor.run(focusEditor("end"));
  });
  effect(() => {
    store.draft();
    slashIndex.set(0);
    mentionIndex.set(0);
  });
  effect(() => {
    const current = mention();
    if (!current) return;
    const timer = setTimeout(() => store.searchFiles(current.query ?? ""), 120);
    return () => clearTimeout(timer);
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
    }, { when: "always" });
  });
  const queue = () => store.currentQueue();
  const showQueue = () => !props.hero && !approval() && !question() && (
    queue().pending.length > 0 || queue().steer.length > 0 || queue().paused || queue().held
  );
  const sendButton = () => (
    <Button
      testID="send-button"
      accessibilityLabel={t("composer.send")}
      disabled={!store.draft().trim() && store.draftImages().length === 0}
      class="h-6 w-6 items-center justify-center rounded-full bg-primary active:bg-primary/90 disabled:opacity-30"
      onPress={() => store.send()}
    >
      <Icon name="arrow-up" size={14} color={token("primary-foreground")} />
    </Button>
  );
  return (
    <View
      class={props.hero ? "p-3 pb-1" : "px-4 pb-1 pt-2"}
      onLayout={({ bounds }) => props.onHeightChange?.(bounds.size.height)}
    >
      <View class="mx-auto w-full" style={{ maxWidth: 860 }}>
        <If when={showQueue()}><QueuePanel {...props} /></If>
        <View
          class={themeStore.classes(
            "composer.root",
            inputFocused()
              ? "gap-2 rounded-2xl border border-ring/40 bg-composer px-2 pb-2 pt-2"
              : "gap-2 rounded-2xl border border-input bg-composer px-2 pb-2 pt-2",
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
            <AttachmentStrip
              {...props}
              onPreview={(index) => {
                editor.blur();
                preview.set(index);
              }}
              onMention={(index) => {
                const image = store.draftImages()[index - 1];
                if (!image) return;
                insertComposerMention(editor, "image", image.id, t("messages.image_label", { index }));
                editor.focus();
              }}
            />
            <View class="relative">
              <If when={slashCommands().length > 0}>
                <SlashCandidates query={slashQuery() ?? ""} selected={slashIndex()} onSelect={chooseSlash} />
              </If>
              <If when={mention()}>
                <MentionCandidates paths={mentionPaths()} selected={mentionIndex()} onSelect={chooseMention} />
              </If>
              <RichEditor
                editor={editor}
                testID="composer-input"
                onFocus={() => inputFocused.set(true)}
                onBlur={() => inputFocused.set(false)}
                onLayout={({ bounds }) => {
                  if (editorWidth === bounds.size.width) return;
                  editorWidth = bounds.size.width;
                  scheduleEditorResize();
                }}
                style={{ height: editorHeight() }}
                onPasteFiles={({ paste }) => {
                  void (async () => {
                    syncDraft();
                    const text = await Clipboard.readText();
                    if (text) editor.insertContent(text);
                    syncDraft();
                    const remaining = Math.max(0, 8 - store.draftImages().length);
                    const pastedImages = paste.files.filter((file) => file.mime.startsWith("image/"));
                    const images = pastedImages.slice(0, remaining);
                    store.addDraftImages(images.map((image) => composerImage({
                      bytes: Array.from(image.data),
                      mime: image.mime || "image/png",
                    })));
                    if (images.length < pastedImages.length) {
                      store.globalError.set(t("composer.image_limit"));
                    }
                    for (const uri of paste.uris) {
                      const path = decodeURIComponent(uri.replace(/^file:\/\//, "").replace(/^\/(?=[A-Za-z]:)/, ""));
                      const duplicateImage = pastedImages.length > 0 && /\.(avif|bmp|gif|heic|heif|ico|jpe?g|png|svg|tiff?|webp)$/i.test(path);
                      if (path && !duplicateImage) {
                        insertComposerMention(editor, "file", path, path.split(/[\\/]/).pop() ?? path);
                      }
                    }
                  })();
                }}
                placeholder={t(queue().paused
                  ? "composer.paused_hint"
                  : running()
                    ? store.config()?.default_delivery === "steer"
                      ? "composer.steer_hint"
                      : "composer.queue_hint"
                    : store.config()?.default_delivery === "steer"
                      ? "composer.steer_keys"
                      : "composer.queue_keys")}
                classes={{
                  content: "min-h-10 p-1 text-sm text-foreground",
                  caret: "bg-foreground",
                  selection: "bg-foreground/20",
                  placeholder: "text-muted-foreground",
                  nodes: {
                    file: "h-[18px] align-text-bottom",
                    image: "h-[18px] align-text-bottom",
                    command: "h-[18px] align-text-bottom",
                    space: "px-0.5",
                  },
                }}
                class={themeStore.classes(
                  "composer.input",
                  "min-h-10 text-foreground",
                )}
              />
            </View>
            <View class="flex-row items-center gap-2">
              <PermissionPicker {...props} />
              <View class="flex-1" />
              <ModelPicker {...props} />
              <If when={running()}>
                <Button
                  testID="stop-button"
                  accessibilityLabel={t("composer.stop")}
                  class="h-6 w-6 items-center justify-center rounded-full bg-destructive"
                  onPress={() => store.stop()}
                >
                  <Icon name="square" size={10} color="#ffffff" />
                </Button>
              </If>
              {sendButton()}
            </View>
          </If>
        </View>
        <SessionStatusBar {...props} />
      </View>
      <ImageViewer
        images={store.draftImages().map((image, index) => ({
          src: image.preview,
          label: t("messages.image_label", { index: index + 1 }),
        }))}
        index={preview()}
        onIndexChange={(index) => preview.set(index)}
        onClose={() => preview.set(null)}
      />
    </View>
  );
}
