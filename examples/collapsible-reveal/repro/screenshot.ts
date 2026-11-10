import type { App } from "nsh/test";

function isScreenshotUnsupported(error: unknown): boolean {
  if (typeof error === "object" && error !== null && "code" in error && error.code === "ERR_NSH_TEST_UNSUPPORTED") return true;
  return String(error).includes("a screenshot needs a real platform");
}

export async function screenshotIfSupported(app: App, path: string): Promise<void> {
  try {
    await app.screenshot(path);
  } catch (error) {
    if (isScreenshotUnsupported(error)) return;
    throw error;
  }
}

export async function supportsScreenshots(app: App): Promise<boolean> {
  try {
    await app.screenshot();
    return true;
  } catch (error) {
    if (isScreenshotUnsupported(error)) return false;
    throw error;
  }
}
