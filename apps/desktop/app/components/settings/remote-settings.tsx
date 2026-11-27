import { Clipboard, Scroll, Text, View, signal, token } from "nsh";
import { Button, Input } from "nsh/base";
import { t } from "../../i18n";
import type { DeviceCodeStartResponse, RelayDeviceInfo } from "../../daemon-client";
import type { UiContext } from "../../ui-context";
import { Icon } from "../icon";

export function RemoteSettings(props: UiContext) {
  const { store } = props;
  const serverUrl = signal(store.relayStatus()?.relay_url || "http://127.0.0.1:8787");
  const deviceName = signal("");
  const username = signal("");
  const password = signal("");
  const approveCode = signal("");
  const approveMessage = signal("");
  const pairing = signal<DeviceCodeStartResponse | null>(null);
  const pairingNotice = signal("");

  const status = () => store.relayStatus();
  const busy = () => store.relayBusy() || store.switchingDevice();
  const activeDevice = () => store.activeDevice();

  const submitAuth = async (mode: "login" | "register") => {
    const url = serverUrl().trim();
    const user = username().trim();
    const pass = password();
    if (!url || !user || !pass) return;
    const ok = mode === "register"
      ? await store.relayRegister(url, user, pass, deviceName().trim() || undefined)
      : await store.relayLogin(url, user, pass, deviceName().trim() || undefined);
    if (ok) {
      password.set("");
      pairing.set(null);
      pairingNotice.set("");
    }
  };

  const startDeviceCode = async () => {
    const url = serverUrl().trim();
    if (!url) return;
    pairingNotice.set("");
    const challenge = await store.relayDeviceStart(url, deviceName().trim() || undefined);
    if (challenge) {
      pairing.set(challenge);
      pairingNotice.set(t("settings.remote.device_code_pending"));
    }
  };

  const pollDeviceCode = async () => {
    const current = pairing();
    if (!current) return;
    const res = await store.relayDevicePoll(
      serverUrl().trim(),
      current.device_code,
      deviceName().trim() || undefined,
    );
    if (!res) return;
    if (res.status === "approved") {
      pairing.set(null);
      pairingNotice.set("");
    } else if (res.status === "expired") {
      pairing.set(null);
      pairingNotice.set(t("settings.remote.device_code_expired"));
    } else {
      pairingNotice.set(t("settings.remote.device_code_pending"));
    }
  };

  const submitApprove = async () => {
    const code = approveCode().trim();
    if (!code) return;
    approveMessage.set("");
    const ok = await store.relayApproveCode(code);
    if (ok) {
      approveCode.set("");
      approveMessage.set(t("settings.remote.approve_success"));
    }
  };

  const isDeviceActive = (device: RelayDeviceInfo): boolean => {
    const active = activeDevice();
    if (active) return active.device_id === device.device_id;
    return status()?.device_id === device.device_id;
  };

  return (
    <View testID="remote-settings" class="relative min-w-0 flex-1 bg-background">
      <Scroll class="flex-1">
        <View class="w-full max-w-[960px] gap-6 p-6">
          <View class="gap-2 border-b border-border pb-4">
            <Text class="text-xl font-semibold text-foreground">{t("settings.remote.title")}</Text>
            <Text class="text-sm text-muted-foreground">{t("settings.remote.description")}</Text>
            <Text class="text-xs text-muted-foreground">{t("settings.remote.privacy_note")}</Text>
          </View>

          <If when={store.relayError().length > 0}>
            <View testID="remote-error" class="rounded-xl border border-destructive/40 bg-destructive/10 px-4 py-3">
              <Text class="text-xs text-destructive">{store.relayError()}</Text>
            </View>
          </If>

          <If when={!status()?.configured}>
            <View testID="remote-auth-card" class="gap-4 rounded-xl border border-border p-4">
              <View class="gap-1.5">
                <Text class="text-xs font-medium text-muted-foreground">{t("settings.remote.server_url")}</Text>
                <Input
                  testID="remote-server-url"
                  value={serverUrl()}
                  placeholder={t("settings.remote.server_url_placeholder")}
                  onChange={({ value }) => serverUrl.set(value)}
                  class="rounded-(--radius) border border-border bg-background px-3 py-2 text-sm text-foreground"
                />
              </View>
              <View class="gap-1.5">
                <Text class="text-xs font-medium text-muted-foreground">{t("settings.remote.device_name")}</Text>
                <Input
                  testID="remote-device-name"
                  value={deviceName()}
                  placeholder={t("settings.remote.device_name_placeholder")}
                  onChange={({ value }) => deviceName.set(value)}
                  class="rounded-(--radius) border border-border bg-background px-3 py-2 text-sm text-foreground"
                />
              </View>
              <View class="flex-row gap-3">
                <View class="min-w-0 flex-1 gap-1.5">
                  <Text class="text-xs font-medium text-muted-foreground">{t("settings.remote.username")}</Text>
                  <Input
                    testID="remote-username"
                    value={username()}
                    onChange={({ value }) => username.set(value)}
                    class="rounded-(--radius) border border-border bg-background px-3 py-2 text-sm text-foreground"
                  />
                </View>
                <View class="min-w-0 flex-1 gap-1.5">
                  <Text class="text-xs font-medium text-muted-foreground">{t("settings.remote.password")}</Text>
                  <Input
                    testID="remote-password"
                    masked
                    value={password()}
                    onChange={({ value }) => password.set(value)}
                    onSubmit={() => void submitAuth("login")}
                    class="rounded-(--radius) border border-border bg-background px-3 py-2 text-sm text-foreground"
                  />
                </View>
              </View>
              <View class="flex-row items-center gap-2 pt-1">
                <Button
                  testID="remote-sign-in"
                  disabled={busy() || !serverUrl().trim() || !username().trim() || !password()}
                  class="rounded-(--radius) bg-primary px-4 py-2 hover:opacity-90 disabled:opacity-40"
                  onPress={() => void submitAuth("login")}
                >
                  <Text class="text-sm font-medium text-primary-foreground">{t("settings.remote.sign_in")}</Text>
                </Button>
                <Button
                  testID="remote-register"
                  disabled={busy() || !serverUrl().trim() || !username().trim() || !password()}
                  class="rounded-(--radius) border border-border px-4 py-2 hover:bg-accent disabled:opacity-40"
                  onPress={() => void submitAuth("register")}
                >
                  <Text class="text-sm text-foreground">{t("settings.remote.register")}</Text>
                </Button>
              </View>
            </View>

            <View testID="remote-device-code-card" class="gap-3 rounded-xl border border-border p-4">
              <View class="gap-0.5">
                <Text class="text-sm font-medium text-foreground">{t("settings.remote.device_code_title")}</Text>
                <Text class="text-xs text-muted-foreground">{t("settings.remote.device_code_description")}</Text>
              </View>
              <If when={pairing() !== null}>
                <View class="gap-2 rounded-lg border border-border bg-muted/40 p-3">
                  <Text class="text-xs text-muted-foreground">
                    {t("settings.remote.device_code_prompt", { url: pairing()!.verification_uri })}
                  </Text>
                  <View class="flex-row items-center gap-3">
                    <Text testID="remote-user-code" class="font-mono text-lg font-semibold text-foreground">
                      {pairing()!.user_code}
                    </Text>
                    <Button
                      class="rounded-(--radius) border border-border px-2.5 py-1 hover:bg-accent"
                      onPress={() => void Clipboard.writeText(pairing()!.user_code)}
                    >
                      <Text class="text-xs text-foreground">{t("common.copy")}</Text>
                    </Button>
                  </View>
                  <If when={pairingNotice().length > 0}>
                    <Text class="text-xs text-muted-foreground">{pairingNotice()}</Text>
                  </If>
                </View>
              </If>
              <View class="flex-row items-center gap-2">
                <Button
                  testID="remote-start-device-code"
                  disabled={busy() || !serverUrl().trim()}
                  class="rounded-(--radius) border border-border px-3 py-1.5 hover:bg-accent disabled:opacity-40"
                  onPress={() => void startDeviceCode()}
                >
                  <Text class="text-sm text-foreground">{t("settings.remote.start_device_code")}</Text>
                </Button>
                <If when={pairing() !== null}>
                  <Button
                    testID="remote-poll-device-code"
                    disabled={busy()}
                    class="rounded-(--radius) bg-primary px-3 py-1.5 hover:opacity-90 disabled:opacity-40"
                    onPress={() => void pollDeviceCode()}
                  >
                    <Text class="text-sm font-medium text-primary-foreground">{t("settings.remote.check_device_code")}</Text>
                  </Button>
                </If>
              </View>
            </View>
          </If>

          <If when={Boolean(status()?.configured)}>
            <View testID="remote-account-card" class="gap-4 rounded-xl border border-border p-4">
              <View class="flex-row items-center justify-between gap-4">
                <View class="min-w-0 flex-1 gap-1">
                  <Text class="text-xs text-muted-foreground">{t("settings.remote.account_section")}</Text>
                  <View class="flex-row items-center gap-2">
                    <View
                      class="h-2.5 w-2.5 rounded-full"
                      style={{ backgroundColor: status()?.connected ? token("primary") : token("warning") }}
                    />
                    <Text class="text-base font-semibold text-foreground">{status()?.username || ""}</Text>
                    <Text class="text-xs text-muted-foreground">
                      {status()?.connected ? t("settings.remote.status_connected") : t("settings.remote.status_connecting")}
                    </Text>
                  </View>
                  <Text class="text-xs text-muted-foreground">
                    {`${status()?.device_name || ""} · ${status()?.relay_url || ""}`}
                  </Text>
                  <If when={Boolean(status()?.last_error)}>
                    <Text class="text-xs text-destructive">{status()?.last_error || ""}</Text>
                  </If>
                </View>
                <View class="flex-row items-center gap-2">
                  <Button
                    testID="remote-refresh"
                    disabled={busy()}
                    class="rounded-(--radius) border border-border px-3 py-1.5 hover:bg-accent disabled:opacity-40"
                    onPress={() => void store.refreshRelayStatus()}
                  >
                    <Text class="text-sm text-foreground">{t("settings.remote.refresh")}</Text>
                  </Button>
                  <Button
                    testID="remote-sign-out"
                    disabled={busy()}
                    class="rounded-(--radius) border border-destructive/40 px-3 py-1.5 hover:bg-destructive/10 disabled:opacity-40"
                    onPress={() => void store.relayLogout()}
                  >
                    <Text class="text-sm text-destructive">{t("settings.remote.sign_out")}</Text>
                  </Button>
                </View>
              </View>
            </View>

            <View class="gap-3 rounded-xl border border-border p-4">
              <Text class="text-sm font-medium text-foreground">{t("settings.remote.approve_title")}</Text>
              <View class="flex-row items-center gap-2">
                <Input
                  testID="remote-approve-input"
                  value={approveCode()}
                  placeholder={t("settings.remote.approve_placeholder")}
                  onChange={({ value }) => approveCode.set(value)}
                  onSubmit={() => void submitApprove()}
                  class="min-w-0 flex-1 rounded-(--radius) border border-border bg-background px-3 py-1.5 font-mono text-sm text-foreground"
                />
                <Button
                  testID="remote-approve-submit"
                  disabled={busy() || !approveCode().trim()}
                  class="rounded-(--radius) bg-primary px-3 py-1.5 hover:opacity-90 disabled:opacity-40"
                  onPress={() => void submitApprove()}
                >
                  <Text class="text-sm font-medium text-primary-foreground">{t("settings.remote.approve_button")}</Text>
                </Button>
              </View>
              <If when={approveMessage().length > 0}>
                <Text class="text-xs text-muted-foreground">{approveMessage()}</Text>
              </If>
            </View>

            <View testID="remote-devices-card" class="gap-3 rounded-xl border border-border p-4">
              <View class="flex-row items-center justify-between">
                <Text class="text-sm font-medium text-foreground">{t("settings.remote.devices_title")}</Text>
                <If when={activeDevice() !== null}>
                  <Button
                    testID="remote-switch-local"
                    disabled={busy()}
                    class="rounded-(--radius) border border-border px-2.5 py-1 hover:bg-accent disabled:opacity-40"
                    onPress={() => void store.selectDevice(null)}
                  >
                    <Text class="text-xs text-foreground">{t("settings.remote.switch_local")}</Text>
                  </Button>
                </If>
              </View>
              <If when={store.relayDevices().length === 0}>
                <Text class="text-xs text-muted-foreground">{t("settings.remote.devices_empty")}</Text>
              </If>
              <Each of={store.relayDevices()} key={(device) => device.device_id}>
                {(device) => {
                  const isSelf = () => status()?.device_id === device().device_id;
                  const active = () => isDeviceActive(device());
                  return (
                    <View
                      testID={`remote-device-${device().device_id}`}
                      class="flex-row items-center justify-between gap-3 rounded-lg border border-border px-3 py-2.5"
                    >
                      <View class="min-w-0 flex-1 flex-row items-center gap-2.5">
                        <Icon name={device().online ? "wifi" : "wifi-off"} size={15} color={device().online ? token("primary") : token("muted-foreground")} />
                        <View class="min-w-0 flex-1 gap-0.5">
                          <View class="flex-row items-center gap-2">
                            <Text class="text-sm font-medium text-foreground">{device().name}</Text>
                            <If when={isSelf()}>
                              <Text class="rounded bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">
                                {t("settings.remote.this_device")}
                              </Text>
                            </If>
                            <If when={active()}>
                              <Text class="rounded bg-primary/15 px-1.5 py-0.5 text-[10px] text-primary">
                                {t("settings.remote.active")}
                              </Text>
                            </If>
                          </View>
                          <Text class="text-xs text-muted-foreground">
                            {`${device().os}/${device().arch} · ${device().online ? t("settings.remote.online") : t("settings.remote.offline")}`}
                          </Text>
                        </View>
                      </View>
                      <View class="flex-row items-center gap-2">
                        <If when={!active() && (device().online || isSelf())}>
                          <Button
                            testID={`remote-connect-${device().device_id}`}
                            disabled={busy()}
                            class="rounded-(--radius) border border-border px-2.5 py-1 hover:bg-accent disabled:opacity-40"
                            onPress={() => void store.selectDevice(isSelf() ? null : device())}
                          >
                            <Text class="text-xs text-foreground">
                              {isSelf() ? t("settings.remote.switch_local") : t("settings.remote.connect")}
                            </Text>
                          </Button>
                        </If>
                        <Button
                          testID={`remote-revoke-${device().device_id}`}
                          disabled={busy()}
                          class="rounded-(--radius) border border-destructive/40 px-2.5 py-1 hover:bg-destructive/10 disabled:opacity-40"
                          onPress={() => void store.relayRevokeDevice(device().device_id)}
                        >
                          <Text class="text-xs text-destructive">{t("settings.remote.revoke")}</Text>
                        </Button>
                      </View>
                    </View>
                  );
                }}
              </Each>
            </View>
          </If>
        </View>
      </Scroll>
    </View>
  );
}
