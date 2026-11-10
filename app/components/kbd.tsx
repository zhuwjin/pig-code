import { Text, View, type JSX } from "nsh";

export type KbdSize = "sm" | "md" | "lg";
export type KbdVariant = "raised" | "outline" | "subtle" | "plain";

const SIZE_CLASS: Record<KbdSize, { box: string; text: string }> = {
  sm: { box: "h-4 min-w-4 px-1", text: "text-[10px]" },
  md: { box: "h-5 min-w-5 px-1.5", text: "text-xs" },
  lg: { box: "h-6 min-w-6 px-2", text: "text-sm" },
};

const VARIANT_CLASS: Record<KbdVariant, string> = {
  raised: "border border-b-2 border-input bg-muted text-foreground",
  outline: "border border-input text-foreground",
  subtle: "bg-muted text-foreground",
  plain: "text-muted-foreground",
};

export function Kbd(props: {
  size?: KbdSize;
  variant?: KbdVariant;
  class?: string;
  testID?: string;
  accessibilityLabel?: string;
  children?: JSX.Element;
}) {
  const size = SIZE_CLASS[props.size ?? "md"];
  const variant = VARIANT_CLASS[props.variant ?? "raised"];
  return (
    <View
      testID={props.testID}
      accessibilityLabel={props.accessibilityLabel}
      class={`shrink-0 flex-row items-center justify-center rounded-md ${size.box} ${variant} ${props.class ?? ""}`}
    >
      <Text
        class={`font-mono font-medium ${size.text}`}
        style={{ numberOfLines: 1, userSelect: "none" }}
      >
        {props.children}
      </Text>
    </View>
  );
}

export function KbdGroup(props: {
  class?: string;
  testID?: string;
  accessibilityLabel?: string;
  children?: JSX.Element;
}) {
  return (
    <View
      testID={props.testID}
      accessibilityLabel={props.accessibilityLabel}
      class={`flex-row items-center gap-1 ${props.class ?? ""}`}
    >
      {props.children}
    </View>
  );
}
