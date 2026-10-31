import { host, mount, signal, Text, View } from "nsh";
import { Button } from "nsh/base";

host.on("tools.call", (call: any) => {
  host.post("tools.result", {
    request_id: call.request_id,
    result: { echo: call.args?.text ?? "", engine: "moon" },
  });
});

host.on("tools.select", (call: any) => {
  host.post("tools.result", {
    request_id: call.request_id,
    result: ["MoonEcho"],
  });
});

host.post("contribution.register", {
  id: "moon-dynamic-note",
  kind: "insertAfter",
  target: "composer.sendButton",
  export: "MoonDynamicNote",
  preserve: [],
});

export default function App() {
  const submitted = signal(false);
  return (
    <View accessibilityLabel="MoonPanel" class="gap-2">
      <Text testID="moon-panel-ready">Moon engine is running</Text>
      <Button
        testID="moon-submit"
        onPress={() => {
          submitted.set(true);
          host.post("composer.submit", {});
        }}
      >
        <Text>{submitted() ? "Submitted through host" : "Submit through host"}</Text>
      </Button>
      <Text accessibilityLabel="MoonDynamicNote" testID="moon-dynamic-note">Dynamic moon note</Text>
    </View>
  );
}

mount(App);
