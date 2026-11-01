import { Image, token, View } from "nsh";
import { Icon } from "../icon";
import { resolveIcon } from "./icon-catalog";

export function ProviderIcon(props: {
  icon?: string | null | (() => string | null | undefined);
  kind?: "provider" | "model";
  size?: number;
}) {
  const meta = () => resolveIcon(typeof props.icon === "function" ? props.icon() : props.icon);
  const size = props.size ?? 20;
  return (
    <If
      when={meta()}
      else={
        <View class="shrink-0 items-center justify-center rounded-md bg-muted" style={{ width: size, height: size }}>
          <Icon
            name={props.kind === "model" ? "brain" : "server"}
            size={Math.round(size * 0.6)}
            color={token("muted-foreground")}
          />
        </View>
      }
    >
      {(value) => (
        <View testID={`provider-icon-${value().id}`} accessibilityLabel={value().title} class="shrink-0" style={{ width: size, height: size }}>
          <Image src={value().asset} resizeMode="contain" class="h-full w-full" />
        </View>
      )}
    </If>
  );
}
