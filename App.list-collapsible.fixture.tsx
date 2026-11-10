import { List, Text, View, mount, signal } from "nsh";
import { Button } from "nsh/base";
import { ListCollapsible } from "./app/components/list-collapsible";

const open = signal(false);
const samples = signal("");
const transitions = signal("");

mount(() => {
  const heights: number[] = [];
  return (
    <View class="h-full w-full">
      <Button testID="toggle" class="h-10" onPress={() => open.update((value) => !value)}>
        <Text>Toggle</Text>
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
          open={open()}
          onTransition={(active) => transitions.update((value) => `${value}${active ? "start" : "end"},`)}
          content={
            <List each={[0]} estimatedItemSize={320} class="h-80">
              {() => <Text>Long content</Text>}
            </List>
          }
        >
          <View />
        </ListCollapsible>
      </View>
      <Text testID="height-samples">{samples()}</Text>
      <Text testID="transition-samples">{transitions()}</Text>
    </View>
  );
});
