import { List, mount, signal, Text, View } from "nsh";
import { Input } from "nsh/base";
import { t } from "./app/i18n";

const JSON_VALUE = `{
  "disabled": {
    "thinking": {
      "type": "disabled"
    }
  },
  "high": {
    "output_config": {
      "effort": "high"
    },
    "thinking": {
      "type": "enabled"
    }
  },
  "max": {
    "output_config": {
      "effort": "max"
    },
    "thinking": {
      "type": "enabled"
    }
  },
  "low": {
    "output_config": {
      "effort": "low"
    },
    "thinking": {
      "type": "enabled"
    }
  }
}`;

type Row = { id: string; kind: "search" | "json" | "card"; value?: number };

const ROWS: Row[] = [
  { id: "search", kind: "search" },
  { id: "json", kind: "json" },
  ...Array.from({ length: 1000 }, (_, index) => ({ id: `card-${index + 1}`, kind: "card" as const, value: index + 1 })),
];

function App() {
  const query = signal("");
  const params = signal(JSON_VALUE);
  return (
    <List
      testID="rows"
      each={ROWS}
      keyExtractor={(row) => row.id}
      estimatedItemSize={86}
      style={{ flex: 1, backgroundColor: "#09090b" }}
      contentStyle={{ padding: 20, paddingTop: 24, gap: 14 }}
    >
      {(row) => (
        <If
          when={row().kind === "search"}
          else={
            <If
              when={row().kind === "json"}
              else={
                <View
                  style={{
                    width: "100%",
                    maxWidth: 560,
                    alignSelf: "center",
                    height: 72,
                    justifyContent: "center",
                    padding: 16,
                    borderWidth: 1,
                    borderColor: "#27272a",
                    borderRadius: 12,
                    backgroundColor: "#18181b",
                  }}
                >
                  <Text style={{ fontSize: 16, color: "#d4d4d8" }}>{`#${row().value}`}</Text>
                </View>
              }
            >
              <View style={{ width: "100%", maxWidth: 560, alignSelf: "center", gap: 14 }}>
                <Text style={{ fontSize: 13, color: "#a1a1aa" }}>{t("settings.models.field.params_json")}</Text>
                <Input
                  testID="json"
                  multiline
                  value={params()}
                  onChange={(text) => params.set(text)}
                  style={{
                    height: 110,
                    padding: 10,
                    borderWidth: 1,
                    borderColor: "#3f3f46",
                    borderRadius: 8,
                    backgroundColor: "#18181b",
                    fontFamily: "Menlo",
                    fontSize: 12,
                    color: "#f4f4f5",
                  }}
                />
              </View>
            </If>
          }
        >
          <View style={{ width: "100%", maxWidth: 560, alignSelf: "center", gap: 14 }}>
            <Text style={{ fontSize: 22, fontWeight: "700", color: "#f4f4f5" }}>{t("settings.appearance.search")}</Text>
            <Input
              testID="search"
              value={query()}
              onChange={(text) => query.set(text)}
              placeholder={t("settings.appearance.search")}
              style={{
                height: 36,
                paddingHorizontal: 12,
                borderWidth: 1,
                borderColor: "#3f3f46",
                borderRadius: 8,
                backgroundColor: "#18181b",
                fontSize: 14,
                color: "#f4f4f5",
                placeholderColor: "#a1a1aa",
              }}
            />
          </View>
        </If>
      )}
    </List>
  );
}

mount(App);
