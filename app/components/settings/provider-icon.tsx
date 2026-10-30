import { Image, token, View } from "nsh";
import { Icon } from "../icon";
import { resolveIcon } from "./icon-catalog";

export function ProviderIcon(props: {
  icon?: string | null;
  kind?: "provider" | "model";
  size?: number;
}) {
  const meta = resolveIcon(props.icon);
  const size = props.size ?? 20;
  if (meta) {
    return (
      <View testID={`provider-icon-${meta.id}`} accessibilityLabel={meta.title} class="shrink-0" style={{ width: size, height: size }}>
        <Image src={meta.asset} resizeMode="contain" class="h-full w-full" />
      </View>
    );
  }
  return (
    <View class="shrink-0 items-center justify-center rounded-md bg-muted" style={{ width: size, height: size }}>
      <Icon
        name={props.kind === "model" ? "brain" : "server"}
        size={Math.round(size * 0.6)}
        color={token("muted-foreground")}
      />
    </View>
  );
}
