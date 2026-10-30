import { Canvas, View, effect, reduceMotion, signal, token } from "nsh";
import type { Point } from "nsh";
import { createParticles, scatterParticles, stepParticles, Vec2 } from "../hero-particles";

const DESIGN_WIDTH = 420;
const DESIGN_HEIGHT = 120;

export function ParticleWordmark() {
  const particles = createParticles();
  const version = signal(0);
  const seed = { value: BigInt(Date.now()) | 1n };
  let pointer: Vec2 | null = null;
  let scattering = false;
  let size = { width: DESIGN_WIDTH, height: DESIGN_HEIGHT };
  let frame: number | undefined;
  let lastTime = 0;

  effect(() => () => cancelAnimationFrame(frame));
  effect(() => {
    if (!reduceMotion()) return;
    for (const particle of particles) {
      particle.position = { ...particle.home };
      particle.velocity = { x: 0, y: 0 };
    }
    version.update((value) => value + 1);
  });

  function schedule() {
    if (frame !== undefined || reduceMotion()) return;
    frame = requestAnimationFrame((time) => {
      frame = undefined;
      const dt = lastTime ? (time - lastTime) / 1000 : 1 / 60;
      lastTime = time;
      const active = stepParticles(particles, pointer, dt, scattering, reduceMotion());
      if (!active) {
        scattering = false;
        lastTime = 0;
      }
      version.update((value) => value + 1);
      if (active) schedule();
    });
  }

  function setPointer(value: Vec2 | null) {
    pointer = value;
    schedule();
  }

  function designPoint(point: Point): Vec2 {
    const scale = Math.min(size.width / DESIGN_WIDTH, size.height / DESIGN_HEIGHT);
    return {
      x: (point.x - (size.width - DESIGN_WIDTH * scale) / 2) / scale,
      y: (point.y - (size.height - DESIGN_HEIGHT * scale) / 2) / scale,
    };
  }

  return (
    <View
      testID="hero-pig-code-wordmark"
      accessibilityLabel="Pig Code"
      role="image"
      class="h-[120px] w-[420px] max-w-full"
      onLayout={(bounds) => {
        size = { width: bounds.size.width, height: bounds.size.height };
      }}
      onHoverMove={(event) => setPointer(designPoint(event.position))}
      onHoverOut={() => setPointer(null)}
      onPress={() => {
        if (reduceMotion()) return;
        scatterParticles(particles, seed);
        pointer = null;
        scattering = true;
        schedule();
      }}
    >
      <Canvas
        class="h-full w-full"
        draw={(cx, canvas) => {
          version();
          const scale = Math.min(canvas.width / DESIGN_WIDTH, canvas.height / DESIGN_HEIGHT);
          const offsetX = (canvas.width - DESIGN_WIDTH * scale) / 2;
          const offsetY = (canvas.height - DESIGN_HEIGHT * scale) / 2;
          for (const particle of particles) {
            const radius = Math.max(0.65, particle.radius * scale);
            cx.globalAlpha = particle.tone === 0 ? 0.9 : 0.72 + particle.tone * 0.03;
            cx.fillStyle = particle.tone === 0 ? token("primary") : token("foreground");
            cx.beginPath();
            cx.arc(
              offsetX + particle.position.x * scale,
              offsetY + particle.position.y * scale,
              radius,
              0,
              Math.PI * 2,
            );
            cx.fill();
          }
        }}
      />
    </View>
  );
}
