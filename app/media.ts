import { Buffer } from "buffer";
import type { ComposerImage, PendingImage } from "./protocol";

let imageId = 0;

export function imageDataUrl(bytes: number[] | ArrayBuffer, mime: string): string {
  const data = bytes instanceof ArrayBuffer ? Buffer.from(bytes) : Buffer.from(bytes);
  return `data:${mime || "image/png"};base64,${data.toString("base64")}`;
}

export function composerImage(image: PendingImage): ComposerImage {
  return {
    ...image,
    id: `image-${++imageId}`,
    preview: imageDataUrl(image.bytes, image.mime),
  };
}

export async function fileComposerImage(file: {
  type: string;
  arrayBuffer(): Promise<ArrayBuffer>;
}): Promise<ComposerImage> {
  const bytes = await file.arrayBuffer();
  const mime = file.type || "image/png";
  return composerImage({ bytes: Array.from(new Uint8Array(bytes)), mime });
}
