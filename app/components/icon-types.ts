export type IconElement =
  | { tag: "path"; d: string }
  | { tag: "circle"; cx: number; cy: number; r: number }
  | { tag: "ellipse"; cx: number; cy: number; rx: number; ry: number }
  | { tag: "rect"; x: number; y: number; width: number; height: number; rx?: number; ry?: number }
  | { tag: "line"; x1: number; y1: number; x2: number; y2: number }
  | { tag: "polyline" | "polygon"; points: string };
