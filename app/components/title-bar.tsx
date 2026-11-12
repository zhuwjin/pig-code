import { View, Text, colorScheme, platform, token } from "nsh";
import { Button } from "nsh/base";
import { titleBar } from "nsh/desktop";
import { t } from "../i18n";
import type { UiContext } from "../ui-context";
import { workspaceName } from "../ui-context";
import { Icon } from "./icon";
import { WINDOW_CONTROLS_WIDTH } from "./window-controls";

export function TitleBar(props: UiContext & {
  sidebarVisible: boolean;
  rightPanelShown: boolean;
  terminalVisible: boolean;
  onToggleSidebar: () => void;
  onToggleRight: () => void;
  onToggleTerminal: () => void;
  onOpenThemes: () => void;
  onSaveTheme: () => void;
}) {
  const { store, themeStore } = props;
  return (
    <View
      testID="title-bar"
      desktop={{ windowDrag: true }}
      style={themeStore.styles("title-bar.root", {
        height: titleBar().height || 44,
        paddingLeft: props.sidebarVisible ? 8 : titleBar().left + 8,
        paddingRight: props.rightPanelShown
          ? 8
          : (platform.os === "windows" ? WINDOW_CONTROLS_WIDTH + 4 : titleBar().right + 8),
        borderBottomWidth: 0.5,
      })}
      class={themeStore.classes(
        "title-bar.root",
        "flex-row items-center border-border bg-background",
      )}
    >
      <View class="min-w-0 flex-1 flex-row items-center gap-2">
        <If when={!props.sidebarVisible}>
          <Button
            testID="sidebar-toggle"
            accessibilityLabel={t("title_bar.expand_sidebar")}
            class="h-8 w-8 shrink-0 items-center justify-center rounded-(--radius) hover:bg-muted active:bg-accent"
            onPress={props.onToggleSidebar}
          >
            <Icon size={14} color={token("secondary-foreground")} name="panel-left" />
          </Button>
        </If>
        <View class="min-w-0 flex-1 flex-row items-center gap-1.5">
          <Text style={{ numberOfLines: 1 }} class="text-sm text-foreground">
            {store.current()
              ? `${workspaceName(store.cwd())} / ${store.currentTitle()}`
              : "Pig Code v0.1.0"}
          </Text>
        </View>
      </View>
      <View class="shrink-0 flex-row items-center gap-1">
        <Button
          testID={colorScheme() === "dark" ? "appearance-dark" : "appearance-light"}
          accessibilityLabel={t("title_bar.toggle_theme")}
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
          accessibilityLabel={t("title_bar.appearance_settings")}
          class="h-8 w-8 items-center justify-center rounded-(--radius) hover:bg-muted active:bg-accent"
          onPress={props.onOpenThemes}
        >
          <Icon size={14} color={token("secondary-foreground")} name="settings" />
        </Button>
        <Button
          testID="terminal-toggle"
          selected={props.terminalVisible}
          accessibilityLabel={t(props.terminalVisible ? "title_bar.close_terminal" : "title_bar.open_terminal")}
          class="h-8 w-8 items-center justify-center rounded-(--radius) hover:bg-muted selected:bg-accent active:bg-accent"
          onPress={props.onToggleTerminal}
        >
          <Icon size={14} color={token("secondary-foreground")} name="square-terminal" />
        </Button>
        <If when={!props.rightPanelShown}>
          <Button
            testID="right-panel-toggle"
            accessibilityLabel={t("title_bar.open_right_panel")}
            class="h-8 w-8 items-center justify-center rounded-(--radius) hover:bg-muted active:bg-accent"
            onPress={props.onToggleRight}
          >
            <Icon size={14} color={token("secondary-foreground")} name="panel-right-open" />
          </Button>
        </If>
      </View>
    </View>
  );
}
