import { platform, token, View } from "nsh";
import { currentWindow } from "nsh/desktop";
import { t } from "../i18n";
import { Icon } from "./icon";

export const WINDOW_CONTROLS_WIDTH = 138;

export function WindowControls() {
  if (platform.os !== "windows") return null;
  const window = currentWindow();
  const maximized = () => window.state()?.mode === "maximized";
  const control = "h-11 w-[46px] items-center justify-center";
  return (
    <View testID="window-controls" class="absolute right-0 top-0 z-50 h-11 flex-row items-center">
      <View
        testID="window-minimize"
        role="button"
        accessibilityLabel={t("window.minimize")}
        desktop={{ windowControl: "minimize" }}
        class={`${control} hover:bg-muted active:bg-accent`}
      >
        <Icon name="minimize" size={14} color={token("secondary-foreground")} />
      </View>
      <View
        testID="window-maximize"
        role="button"
        accessibilityLabel={t(maximized() ? "window.restore" : "window.maximize")}
        desktop={{ windowControl: "maximize" }}
        class={`${control} hover:bg-muted active:bg-accent`}
      >
        <Icon name={maximized() ? "copy" : "maximize"} size={14} color={token("secondary-foreground")} />
      </View>
      <View
        testID="window-close"
        role="button"
        accessibilityLabel={t("window.close")}
        desktop={{ windowControl: "close" }}
        class={`${control} hover:bg-destructive active:bg-destructive`}
      >
        <Icon name="x" size={14} color={token("secondary-foreground")} />
      </View>
    </View>
  );
}

export function WindowResizeEdges() {
  if (platform.os !== "windows") return null;
  const edge = 5;
  const corner = 12;
  return (
    <>
      <View testID="window-resize-top" style={{ position: "absolute", top: 0, left: corner, right: corner, height: edge }} desktop={{ windowResize: "top" }} />
      <View testID="window-resize-bottom" style={{ position: "absolute", bottom: 0, left: corner, right: corner, height: edge }} desktop={{ windowResize: "bottom" }} />
      <View testID="window-resize-left" style={{ position: "absolute", left: 0, top: corner, bottom: corner, width: edge }} desktop={{ windowResize: "left" }} />
      <View testID="window-resize-right" style={{ position: "absolute", right: 0, top: corner, bottom: corner, width: edge }} desktop={{ windowResize: "right" }} />
      <View testID="window-resize-top-left" style={{ position: "absolute", top: 0, left: 0, width: corner, height: corner }} desktop={{ windowResize: "topLeft" }} />
      <View testID="window-resize-top-right" style={{ position: "absolute", top: 0, right: 0, width: corner, height: corner }} desktop={{ windowResize: "topRight" }} />
      <View testID="window-resize-bottom-left" style={{ position: "absolute", bottom: 0, left: 0, width: corner, height: corner }} desktop={{ windowResize: "bottomLeft" }} />
      <View testID="window-resize-bottom-right" style={{ position: "absolute", bottom: 0, right: 0, width: corner, height: corner }} desktop={{ windowResize: "bottomRight" }} />
    </>
  );
}
