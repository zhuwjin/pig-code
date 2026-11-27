import { Text } from "nsh";
import { Tab, Tabs } from "nsh/base";

export function Segmented<T extends string>(props: {
  options: readonly { value: T; label: string; count?: number }[];
  value: T;
  onChange: (value: T) => void;
  class?: string;
  tabClass?: string;
  testID?: string;
}) {
  return (
    <Tabs
      testID={props.testID}
      class={`flex-row items-center rounded-md border border-border bg-muted p-0.5 ${props.class ?? ""}`}
      indicatorClass="inset-y-0.5 rounded border border-border bg-background dark:border-foreground/10 dark:bg-foreground/15"
    >
      <Each of={props.options} key={(option) => option.value}>
        {(option) => (
          <Tab
            testID={props.testID ? `${props.testID}-${option().value}` : undefined}
            selected={props.value === option().value}
            class={`group flex-row items-center justify-center gap-1 rounded px-2 py-1 ${props.tabClass ?? ""}`}
            onPress={() => props.onChange(option().value)}
          >
            <Text class="text-xs font-medium text-muted-foreground group-selected:text-foreground">
              {option().label}
            </Text>
            <If when={option().count !== undefined}>
              <Text class="text-[10px] text-muted-foreground" style={{ fontVariant: ["tabular-nums"] }}>
                {option().count}
              </Text>
            </If>
          </Tab>
        )}
      </Each>
    </Tabs>
  );
}
