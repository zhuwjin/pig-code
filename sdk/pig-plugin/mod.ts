// The public `pig/plugin` module. Point the `pig/plugin` alias in your app's
// `nsh.json` at this file (see README.md).

export * from "./index";
export { createTestHost, rejectedAsPluginError } from "./test-host";
export type { ServiceRequestRecord, TestHost, TestHostOptions } from "./test-host";
