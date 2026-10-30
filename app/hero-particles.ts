import { WORDMARK_POINTS } from "./wordmark-points";

export interface Vec2 {
  x: number;
  y: number;
}

export interface Particle {
  home: Vec2;
  position: Vec2;
  velocity: Vec2;
  radius: number;
  tone: number;
}

export function createParticles(): Particle[] {
  return WORDMARK_POINTS.map(([x, y, radius, tone]) => ({
    home: { x, y },
    position: { x, y },
    velocity: { x: 0, y: 0 },
    radius,
    tone,
  }));
}

export function scatterParticles(particles: Particle[], seed: { value: bigint }): void {
  for (const particle of particles) {
    const angle = random(seed) * Math.PI * 2;
    const speed = 220 + random(seed) * 320;
    particle.velocity.x = Math.cos(angle) * speed;
    particle.velocity.y = Math.sin(angle) * speed;
    particle.position.x += Math.cos(angle) * (2 + random(seed) * 4);
    particle.position.y += Math.sin(angle) * (2 + random(seed) * 4);
  }
}

export function stepParticles(
  particles: Particle[],
  pointer: Vec2 | null,
  deltaSeconds: number,
  scattering: boolean,
  reduceMotion: boolean,
): boolean {
  const spring = scattering ? 28 : 210;
  const damping = scattering ? 4 : 13;
  const repulsion = scattering ? 3200 * (28 / 210) : 3200;
  const dt = Math.max(0.0001, Math.min(deltaSeconds, 0.033));
  let active = false;
  for (let index = 0; index < particles.length; index += 1) {
    const particle = particles[index];
    if (reduceMotion) {
      particle.position = { ...particle.home };
      particle.velocity = { x: 0, y: 0 };
      continue;
    }
    let ax = (particle.home.x - particle.position.x) * spring;
    let ay = (particle.home.y - particle.position.y) * spring;
    if (pointer) {
      const dx = particle.position.x - pointer.x;
      const dy = particle.position.y - pointer.y;
      const distance = Math.hypot(dx, dy);
      if (distance < 76) {
        const direction = distance > 0.001
          ? { x: dx / distance, y: dy / distance }
          : { x: Math.cos(index * 2.399963), y: Math.sin(index * 2.399963) };
        const strength = (1 - distance / 76) * repulsion;
        ax += direction.x * strength;
        ay += direction.y * strength;
      }
    }
    particle.velocity.x = (particle.velocity.x + ax * dt) * Math.exp(-damping * dt);
    particle.velocity.y = (particle.velocity.y + ay * dt) * Math.exp(-damping * dt);
    particle.position.x += particle.velocity.x * dt;
    particle.position.y += particle.velocity.y * dt;
    if (Math.hypot(particle.velocity.x, particle.velocity.y) > 0.05 || Math.hypot(ax, ay) > 0.5) {
      active = true;
    }
  }
  if (!active && !pointer) {
    for (const particle of particles) {
      particle.position = { ...particle.home };
      particle.velocity = { x: 0, y: 0 };
    }
  }
  return active;
}

function random(seed: { value: bigint }): number {
  const mask = 0xffffffffffffffffn;
  let value = seed.value;
  value ^= value >> 12n;
  value ^= (value << 25n) & mask;
  value ^= value >> 27n;
  seed.value = value & mask;
  const next = (seed.value * 0x2545f4914f6cdd1dn) & mask;
  return Number(next >> 11n) / 9007199254740992;
}
