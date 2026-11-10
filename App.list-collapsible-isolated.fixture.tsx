import { mount, signal, Text, View } from "nsh";
import { Button } from "nsh/base";
import { ListCollapsible } from "./app/components/list-collapsible";

const expanded = signal<Record<string, boolean>>({});
const samples = signal("");
const transitions = signal("");

function toggle(id: string): void {
  expanded.update((all) => ({ ...all, [id]: !(all[id] ?? false) }));
}

mount(() => {
  const heights: number[] = [];
  return (
    <View class="h-full w-full">
      <Button testID="first" class="h-10" onPress={() => toggle("first")}>
        <Text>First</Text>
      </Button>
      <Button testID="other" class="h-10" onPress={() => toggle("other")}>
        <Text>Other</Text>
      </Button>
      <View
        onLayout={({ bounds }) => {
          const value = Math.round(bounds.size.height * 100) / 100;
          if (heights[heights.length - 1] === value) return;
          heights.push(value);
          samples.set(heights.join(","));
        }}
      >
        <ListCollapsible
          open={expanded()["first"] ?? false}
          onTransition={(active) => transitions.update((value) => `${value}${active ? "start" : "end"},`)}
          content={<View style={{ height: 320 }}><Text>First content</Text></View>}
        >
          <View />
        </ListCollapsible>
      </View>
      <ListCollapsible
        open={expanded()["other"] ?? false}
        content={<View style={{ height: 160 }}><Text>Other content</Text></View>}
      >
        <View />
      </ListCollapsible>
      <Text testID="height-samples">{samples()}</Text>
      <Text testID="transition-samples">{transitions()}</Text>
    </View>
  );
});
