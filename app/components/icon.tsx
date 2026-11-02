import {
  Canvas,
  CanvasRenderingContext2D,
  Image,
  Path2D,
  token,
} from "nsh";
import pigIcon from "../../assets/icons/pig.svg";
import { ICON_ELEMENTS } from "./icon-elements";
import type { IconElement } from "./icon-types";

export type IconName = keyof typeof ICON_ELEMENTS | "pig";

export function iconTint(name: IconName, color?: string): string | undefined {
  return name === "pig" ? undefined : (color ?? token("foreground"));
}

function drawElement(cx: CanvasRenderingContext2D, element: IconElement): void {
  if (element.tag === "path") {
    if (element.filled) cx.fill(new Path2D(element.d));
    cx.stroke(new Path2D(element.d));
    return;
  }
  switch (element.tag) {
    case "circle":
      cx.beginPath();
      cx.arc(element.cx, element.cy, element.r, 0, Math.PI * 2);
      break;
    case "ellipse":
      cx.beginPath();
      cx.ellipse(element.cx, element.cy, element.rx, element.ry, 0, 0, Math.PI * 2);
      break;
    case "rect":
      cx.beginPath();
      cx.roundRect(
        element.x,
        element.y,
        element.width,
        element.height,
        element.rx ?? element.ry ?? 0,
      );
      break;
    case "line":
      cx.beginPath();
      cx.moveTo(element.x1, element.y1);
      cx.lineTo(element.x2, element.y2);
      break;
    case "polyline":
    case "polygon": {
      const points = element.points.trim().split(/[\s,]+/).map(Number);
      cx.beginPath();
      cx.moveTo(points[0], points[1]);
      for (let index = 2; index < points.length; index += 2) {
        cx.lineTo(points[index], points[index + 1]);
      }
      if (element.tag === "polygon") cx.closePath();
      break;
    }
  }
  if (element.filled) cx.fill();
  cx.stroke();
}

export function paintIcon(
  cx: CanvasRenderingContext2D,
  elements: readonly IconElement[],
  tint: string,
  width: number,
): void {
  cx.scale(width / 24, width / 24);
  cx.fillStyle = tint;
  cx.strokeStyle = tint;
  cx.lineWidth = 2;
  cx.lineCap = "round";
  cx.lineJoin = "round";
  for (const element of elements) drawElement(cx, element);
}

export function Icon(props: {
  name: IconName;
  color?: string;
  size?: number;
  class?: string;
  testID?: string;
}) {
  const size = props.size ?? 16;
  if (props.name === "pig") {
    return (
      <Image
        testID={props.testID}
        src={pigIcon}
        accessibilityLabel="Pig Code"
        resizeMode="contain"
        class={props.class ?? "shrink-0"}
        style={{ width: size, height: size }}
      />
    );
  }
  const elements = ICON_ELEMENTS[props.name as keyof typeof ICON_ELEMENTS];
  return (
    <Canvas
      testID={props.testID}
      class={props.class ?? "shrink-0"}
      pointerEvents="none"
      style={{ width: size, height: size }}
      draw={(cx, { width }) => {
        paintIcon(cx, elements, iconTint(props.name, props.color)!, width);
      }}
    />
  );
}
