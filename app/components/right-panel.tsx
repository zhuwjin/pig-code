import { Scroll, Text, View, effect, token } from "nsh";
import { Button, DiffView } from "nsh/base";
import type { GitFileChange } from "../protocol";
import type { UiContext } from "../ui-context";
import { Icon } from "./icon";

function ChangeSection(props: UiContext & {
  title: string;
  changes: GitFileChange[];
  staged: boolean;
}) {
  return (
    <View class="gap-1">
      <Text class="px-3 pb-1 pt-3 text-xs font-medium text-muted-foreground">{props.title}</Text>
      <Each of={props.changes} key={(change) => `${props.staged}:${change.path}`}>
        {(change) => (
          <Button
            testID={`git-change-${props.staged ? "staged" : "unstaged"}-${change().path}`}
            class="flex-row items-center gap-2 rounded-(--radius) px-3 py-2 hover:bg-accent"
            onPress={() => props.store.openGitDiff(change().path, props.staged)}
          >
            <Text class="w-4 shrink-0 text-xs text-muted-foreground">{change().status}</Text>
            <Text numberOfLines={1} class="min-w-0 flex-1 text-sm text-foreground">{change().path}</Text>
            <Text class="text-xs text-success">+{change().additions}</Text>
            <Text class="text-xs text-destructive">-{change().deletions}</Text>
          </Button>
        )}
      </Each>
    </View>
  );
}

export function RightPanel(props: UiContext & { onClose: () => void }) {
  const { store, themeStore } = props;
  effect(() => {
    store.cwd();
    store.closeGitDiff();
    store.refreshGitStatus();
  });
  const status = () => store.gitStatus();
  const diff = () => store.gitDiff();
  return (
    <View testID="right-panel" class="w-full flex-1 bg-background">
      <View class="h-10 shrink-0 flex-row items-center gap-2 border-b border-border px-3">
        <Text class="flex-1 text-sm font-medium text-foreground">{diff() ? diff()!.path : "文件变更"}</Text>
        <If when={diff()}>
          <Button
            testID="git-diff-back"
            class="rounded-(--radius) px-2 py-1 hover:bg-accent"
            onPress={() => store.closeGitDiff()}
          >
            <Text class="text-xs text-muted-foreground">返回</Text>
          </Button>
        </If>
        <Button
          testID="right-refresh"
          accessibilityLabel="刷新变更"
          class="h-7 w-7 items-center justify-center rounded-(--radius) hover:bg-accent"
          onPress={() => store.refreshGitStatus()}
        >
          <Icon name="refresh-cw" size={13} color={token("muted-foreground")} />
        </Button>
        <Button
          testID="right-panel-close"
          accessibilityLabel="关闭右侧面板"
          class="h-7 w-7 items-center justify-center rounded-(--radius) hover:bg-accent"
          onPress={props.onClose}
        >
          <Icon name="x" size={14} color={token("muted-foreground")} />
        </Button>
      </View>
      <If when={diff()}>
        {(value) => (
          <View class="min-h-0 flex-1">
            <If when={value().note}>
              <Text class="px-3 py-2 text-xs text-muted-foreground">
                {value().note === "Binary" ? "二进制文件无法显示文本 diff" : value().note === "TooLarge" ? "文件过大，无法显示 diff" : "diff 已截断"}
              </Text>
            </If>
            <View class={themeStore.classes("diff.root", "min-h-0 flex-1")}>
              <DiffView
                patch={value().diff}
                language={value().path}
                layout="unified"
                softWrap
                classes={{
                  gutter: themeStore.classes("diff.gutter", "bg-tool-surface"),
                  lineAdded: themeStore.classes("diff.line-added", "bg-diff-added"),
                  lineDeleted: themeStore.classes("diff.line-deleted", "bg-diff-deleted"),
                  wordAdded: themeStore.classes("diff.word-added", "bg-diff-added-word"),
                  wordDeleted: themeStore.classes("diff.word-deleted", "bg-diff-deleted-word"),
                  indicatorAdded: "bg-success",
                  indicatorDeleted: "bg-destructive",
                  separator: "bg-muted text-muted-foreground",
                  fileHeader: "bg-secondary text-secondary-foreground",
                }}
              />
            </View>
          </View>
        )}
      </If>
      <If when={!diff() && !status()}>
        <View class="flex-1 items-center justify-center">
          <Text class="text-sm text-muted-foreground">正在读取变更…</Text>
        </View>
      </If>
      <If when={!diff() && status()?.is_git === false}>
        <View class="flex-1 items-center justify-center px-6">
          <Text class="text-center text-sm text-muted-foreground">当前目录不是 Git 仓库</Text>
        </View>
      </If>
      <If when={!diff() && status()?.is_git}>
        <Scroll class="flex-1" contentClass="p-2">
          <ChangeSection {...props} title="未暂存" changes={status()!.unstaged} staged={false} />
          <ChangeSection {...props} title="已暂存" changes={status()!.staged} staged />
          <If when={status()!.unstaged.length + status()!.staged.length === 0}>
            <Text class="px-3 py-8 text-center text-sm text-muted-foreground">没有文件变更</Text>
          </If>
        </Scroll>
      </If>
    </View>
  );
}
