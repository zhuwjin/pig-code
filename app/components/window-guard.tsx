import { host, nativeComponent } from "nsh";

const NativeWindowGuard = nativeComponent("pig/window-guard", {
  props: [],
});

export function WindowGuard() {
  const enabled = host.available() && host.params.windowGuard === true;
  return (
    <If when={enabled}>
      <NativeWindowGuard testID="window-guard" />
    </If>
  );
}
