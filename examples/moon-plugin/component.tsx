import { Text, View } from "nsh";

export function MoonPage() {
  return (
    <View testID="moon-page" class="min-h-0 flex-1 gap-4 bg-background p-6">
      <Text class="text-2xl font-semibold text-foreground">Moon workspace</Text>
      <Text class="text-sm text-muted-foreground">This page is rendered in the host Engine.</Text>
    </View>
  );
}

export function MoonSettings() {
  return (
    <View testID="moon-settings" class="min-h-0 flex-1 gap-3 bg-background p-6">
      <Text class="text-xl font-semibold text-foreground">Moon settings</Text>
      <Text class="text-sm text-muted-foreground">This settings page is rendered in the host Engine.</Text>
    </View>
  );
}

export function MoonPopover() {
  return (
    <View testID="moon-popover" class="gap-3 bg-popover p-3">
      <Text class="text-sm font-semibold text-popover-foreground">Moon details</Text>
      <Text class="text-sm text-muted-foreground">This popover shares the host View tree.</Text>
    </View>
  );
}
