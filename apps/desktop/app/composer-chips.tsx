import { Text, token, View } from "nsh";
import { defineExtension, type RichExtension, type RichNodeViewProps } from "nsh/base";
import { Icon, type IconName } from "./components/icon";

type ChipKind = "file" | "image" | "command";

const CHIP_CONFIG: Record<ChipKind, { icon: IconName; trigger: string; prefix: string }> = {
  file: { icon: "file-text", trigger: "@", prefix: "@" },
  image: { icon: "image", trigger: "", prefix: "" },
  command: { icon: "square-terminal", trigger: "", prefix: "" },
};

function chipExtension(kind: ChipKind): RichExtension {
  const { icon, trigger, prefix } = CHIP_CONFIG[kind];
  return defineExtension({
    name: kind,
    node: {
      group: "inline",
      inline: true,
      atom: true,
      selectable: false,
      renderText: "{mentionSuggestionChar}{label}",
      attrs: {
        id: { default: null },
        label: { default: null },
        mentionSuggestionChar: { default: trigger },
      },
    },
    addNodeView: () => (props: RichNodeViewProps) => {
      const label = () => {
        const raw = typeof props.node.attrs?.label === "string" ? props.node.attrs.label : "";
        return `${prefix}${raw}`;
      };
      return (
        <View
          testID={`composer-chip-${kind}`}
          class="align-text-bottom h-5 flex-row items-center gap-1 rounded bg-accent px-1"
          style={{ transform: [{ translateY: -0.67 }] }}
        >
          <Icon
            testID={`composer-chip-icon-${kind}`}
            name={icon}
            size={12}
            color={token("primary")}
          />
          <Text class="text-sm text-primary">{label()}</Text>
        </View>
      );
    },
  });
}

const spaceExtension: RichExtension = defineExtension({
  name: "space",
  node: {
    group: "inline",
    inline: true,
    atom: true,
    selectable: false,
    renderText: "",
    attrs: {
      id: { default: "" },
      label: { default: "" },
      mentionSuggestionChar: { default: "" },
    },
  },
});

export const composerChipExtensions: RichExtension[] = [
  chipExtension("file"),
  chipExtension("image"),
  chipExtension("command"),
  spaceExtension,
];
