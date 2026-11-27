type Rgb = [number, number, number];

function channels(hex: string): Rgb {
  const value = hex.replace("#", "");
  const normalized = value.length === 3 || value.length === 4
    ? [...value].map((part) => part + part).join("")
    : value;
  const full = normalized.length === 6 ? `${normalized}ff` : normalized;
  if (!/^[0-9a-fA-F]{8}$/.test(full)) return [0, 0, 0];
  return [0, 2, 4].map((index) => parseInt(full.slice(index, index + 2), 16) / 255) as Rgb;
}

function hex([r, g, b]: Rgb): string {
  const channel = (value: number) => Math.round(Math.min(1, Math.max(0, value)) * 255).toString(16).padStart(2, "0");
  return `#${channel(r)}${channel(g)}${channel(b)}`;
}

function lightness([r, g, b]: Rgb): number {
  return (Math.max(r, g, b) + Math.min(r, g, b)) / 2;
}

function linear(value: number): number {
  return value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4;
}

function gamma(value: number): number {
  return value <= 0.0031308 ? 12.92 * value : 1.055 * value ** (1 / 2.4) - 0.055;
}

function mixOklab(text: Rgb, target: Rgb, share: number): Rgb {
  const a = text.map(linear);
  const b = target.map(linear);
  const l = [0.4122214708, 0.5363325363, 0.0514459929];
  const m = [0.2119034982, 0.6806995451, 0.1073969566];
  const s = [0.0883024619, 0.2817188376, 0.6299787005];
  const cubeRoot = (value: number) => Math.cbrt(value);
  const lab = (rgb: Rgb) => {
    const ll = cubeRoot(rgb[0] * l[0] + rgb[1] * l[1] + rgb[2] * l[2]);
    const mm = cubeRoot(rgb[0] * m[0] + rgb[1] * m[1] + rgb[2] * m[2]);
    const ss = cubeRoot(rgb[0] * s[0] + rgb[1] * s[1] + rgb[2] * s[2]);
    return [
      0.2104542553 * ll + 0.793617785 * mm + 0.004072619 * ss,
      1.9779984951 * ll - 2.428592205 * mm + 0.4505937099 * ss,
      0.0259040371 * ll + 0.7827717662 * mm - 0.808675766 * ss,
    ] as Rgb;
  };
  const textLab = lab(a as Rgb);
  const targetLab = lab(b as Rgb);
  const mixed = textLab.map((value, index) => value + (targetLab[index] - value) * share) as Rgb;
  const [L, A, B] = mixed;
  const ll = (L + 0.3963377774 * A + 0.2158037573 * B) ** 3;
  const mm = (L - 0.1055613458 * A - 0.0638541728 * B) ** 3;
  const ss = (L - 0.0894841775 * A - 1.291485548 * B) ** 3;
  return [
    4.0767416621 * ll - 3.3077115913 * mm + 0.2309699292 * ss,
    -1.2684380046 * ll + 2.6097574011 * mm - 0.3413193965 * ss,
    -0.0041960863 * ll - 0.7034186147 * mm + 1.707614701 * ss,
  ].map(gamma) as Rgb;
}

export function shimmerHighlightColor(
  text: string,
  background: string,
  foreground: string,
  dark: boolean,
  override?: string,
): string {
  if (override) return override;
  const textRgb = channels(text);
  const targetRgb = channels(dark ? foreground : background);
  const oppositeRgb = channels(dark ? background : foreground);
  const target = Math.abs(lightness(targetRgb) - lightness(textRgb)) < 0.1 ? oppositeRgb : targetRgb;
  return hex(mixOklab(textRgb, target, 0.2));
}

export function shimmerLayerOpacity(dark: boolean, layerCount: number): number {
  const peak = dark ? 0.6 : 0.75;
  return 1 - (1 - peak) ** (1 / layerCount);
}
