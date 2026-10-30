import { View, Text, colorScheme, token } from "nsh";
import { Button } from "nsh/base";
import { titleBar } from "nsh/desktop";
import type { UiContext } from "../ui-context";
import { workspaceName } from "../ui-context";
import { Icon } from "./icon";

export function TitleBar(props: UiContext & {
  sidebarVisible: boolean;
  onToggleSidebar: () => void;
  onOpenThemes: () => void;
  onSaveTheme: () => void;
}) {
  const { store, themeStore } = props;
  return (
    <View
      testID="title-bar"
      desktop={{ windowDrag: true }}
      style={themeStore.styles("title-bar.root", {
        height: titleBar().height || 45,
        paddingLeft: props.sidebarVisible ? 12 : titleBar().left + 8,
        paddingRight: titleBar().right + 8,
      })}
      class={themeStore.classes(
        "title-bar.root",
        "flex-row items-center gap-2 border-b border-border bg-title-bar",
      )}
    >
      <If when={!props.sidebarVisible}>
        <Button
          testID="sidebar-toggle"
          accessibilityLabel="展开侧边栏"
          class="h-8 w-8 shrink-0 items-center justify-center rounded-(--radius) hover:bg-muted active:bg-accent"
          onPress={props.onToggleSidebar}
        >
          <Icon size={14} color={token("secondary-foreground")} name="panel-left" />
        </Button>
      </If>
      <View class="min-w-0 flex-1 flex-row items-center gap-1.5">
        <Text numberOfLines={1} class="text-sm text-foreground">
          {store.current()
            ? `${workspaceName(store.cwd())} / ${store.currentTitle()}`
            : "Pig Code v0.1.0"}
        </Text>
      </View>
      <Button
        testID={colorScheme() === "dark" ? "appearance-dark" : "appearance-light"}
        accessibilityLabel="切换明暗主题"
        class="h-8 w-8 items-center justify-center rounded-(--radius) hover:bg-muted active:bg-accent"
        onPress={() => {
          themeStore.setMode(colorScheme() === "dark" ? "light" : "dark");
          props.onSaveTheme();
        }}
      >
        <Icon size={14} color={token("secondary-foreground")} name={colorScheme() === "dark" ? "sun" : "moon"} />
      </Button>
      <Button
        testID="title-theme-settings"
        accessibilityLabel="外观与主题设置"
        class="h-8 w-8 items-center justify-center rounded-(--radius) hover:bg-muted active:bg-accent"
        onPress={props.onOpenThemes}
      >
        <Icon size={14} color={token("secondary-foreground")} name="settings" />
      </Button>
      <Button
        disabled
        accessibilityLabel="终端面板（尚未迁移）"
        class="h-8 w-8 items-center justify-center rounded-(--radius)"
      >
        <Icon size={14} color={token("secondary-foreground")} name="square-terminal" />
      </Button>
      <Button
        disabled
        accessibilityLabel="右侧面板（尚未迁移）"
        class="h-8 w-8 items-center justify-center rounded-(--radius)"
      >
        <Icon size={14} color={token("secondary-foreground")} name="panel-right-close" />
      </Button>
    </View>
  );
}
