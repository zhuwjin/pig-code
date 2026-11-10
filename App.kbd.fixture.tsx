import { mount, Text, View } from "nsh";
import { Kbd, KbdGroup } from "./app/components/kbd";

mount(() => (
  <View class="h-full w-full gap-5 bg-background p-6">
    <View class="flex-row items-center gap-2">
      <Kbd testID="kbd-sm" size="sm">K</Kbd>
      <Kbd testID="kbd-md">K</Kbd>
      <Kbd testID="kbd-lg" size="lg">K</Kbd>
    </View>
    <View class="flex-row items-center gap-2">
      <Kbd testID="kbd-raised">K</Kbd>
      <Kbd testID="kbd-outline" variant="outline">K</Kbd>
      <Kbd testID="kbd-subtle" variant="subtle">K</Kbd>
      <Kbd testID="kbd-plain" variant="plain">K</Kbd>
    </View>
    <KbdGroup testID="kbd-group" accessibilityLabel="Command K">
      <Kbd>⌘</Kbd>
      <Text class="text-xs text-muted-foreground">+</Text>
      <Kbd>K</Kbd>
    </KbdGroup>
  </View>
));
