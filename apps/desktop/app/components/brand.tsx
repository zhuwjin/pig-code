import { Text, View } from "nsh";
import { Icon } from "./icon";

export function BrandMark(props: { size?: number }) {
  const size = props.size ?? 20;
  return (
    <View
      testID="brand-mark"
      class="shrink-0 items-center justify-center rounded-md border border-border bg-background"
      style={{ width: size, height: size }}
    >
      <Icon name="pig" size={size * 0.8} />
    </View>
  );
}

export function SidebarBrand() {
  return (
    <View testID="sidebar-brand" class="min-w-0 flex-row items-center gap-2">
      <BrandMark />
      <Text
        testID="sidebar-brand-name"
        style={{ numberOfLines: 1 }}
        class="text-sm font-semibold text-foreground"
      >
        Pig Code
      </Text>
    </View>
  );
}
