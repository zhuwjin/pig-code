import { colorScheme, Image, token, View } from "nsh";
import { Button } from "nsh/base";
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
      {(value) => {
        const asset = () => colorScheme() === "dark" ? value().darkAsset ?? value().asset : value().asset;
        return (
          <View testID={`provider-icon-${value().id}`} accessibilityLabel={value().title} class="shrink-0" style={{ width: size, height: size }}>
            <Image src={asset()} resizeMode="contain" class="h-full w-full" />
          </View>
        );
      }}
    </If>
  );
}

export function ProviderIconButton(props: {
  testID?: string;
  accessibilityLabel: string;
  icon?: string | null | (() => string | null | undefined);
  kind?: "provider" | "model";
  size?: number;
  buttonClass?: string;
  onPress: () => void;
}) {
  const size = props.size ?? 20;
  return (
    <Button
      testID={props.testID}
      accessibilityLabel={props.accessibilityLabel}
      class={`group relative ${props.buttonClass ?? "rounded-md p-0.5"}`}
      onPress={props.onPress}
    >
      <ProviderIcon icon={props.icon} kind={props.kind} size={size} />
      <View class="absolute inset-0 items-center justify-center rounded-md bg-black/30 opacity-0 group-hover:opacity-100">
        <Icon name="pencil" size={Math.round(size * 0.5)} color="#fff" />
      </View>
    </Button>
  );
}
