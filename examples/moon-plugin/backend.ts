import { apply } from "pig/plugin";

import { moonPlugin } from "./applier";

// The package's backend entry: the official SDK specifier (`pig/plugin`,
// mapped to the SDK in nsh.json) declares the applier; the implementation lives
// in ./applier.ts.
apply(moonPlugin);

export { moonPlugin };
