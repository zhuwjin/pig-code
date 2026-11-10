import { mount, signal, Text, View } from "nsh";
import { Button } from "nsh/base";
import { ListCollapsible } from "./app/components/list-collapsible";

const id = signal("a");
const mounted = signal(true);
const contentHeight = signal(320);
const samples = signal("");

mount(() => {
  const heights: number[] = [];
  return (
    <View class="h-full w-full">
      <Button
        testID="switch"
        class="h-10"
        onPress={() => {
          id.update((value) => value === "a" ? "b" : "a");
          contentHeight.update((value) => value === 320 ? 160 : 320);
        }}
      >
        <Text>Switch identity</Text>
      </Button>
      <Button
        testID="resize"
        class="h-10"
        onPress={() => contentHeight.update((value) => value === 320 ? 160 : 320)}
      >
        <Text>Resize content</Text>
      </Button>
      <Button testID="recycle" class="h-10" onPress={() => mounted.update((value) => !value)}>
        <Text>Recycle</Text>
      </Button>
      <View
        onLayout={({ bounds }) => {
          const value = Math.round(bounds.size.height * 100) / 100;
          if (heights[heights.length - 1] === value) return;
          heights.push(value);
          samples.set(heights.join(","));
        }}
      >
        <If when={mounted()}>
          <ListCollapsible
            open={true}
            testID={id()}
            layoutVersion="test"
            version={contentHeight()}
            content={<View style={{ height: contentHeight() }}><Text>Content</Text></View>}
          >
            <View />
          </ListCollapsible>
        </If>
      </View>
      <Text testID="height-samples">{samples()}</Text>
    </View>
  );
});
