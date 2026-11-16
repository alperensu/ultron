import type { Transition } from "motion/react";

/** Kullanıcı hareketi azaltmak istiyor mu (prefers-reduced-motion; WebView2'de Windows ayarı). */
export function reducedMotion(): boolean {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return false;
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

const SPRING_EASE = [0.32, 0.72, 0, 1] as const;

/** iOS yayı: cubic-bezier(0.32,0.72,0,1). Hareket azaltılmışsa süre sıfır. */
export function spring(duration = 0.42): Transition {
  if (reducedMotion()) return { duration: 0 };
  return { duration, ease: [...SPRING_EASE] };
}

/** Liste girişi için kademeli gecikme (saniye). Hareket azaltılmışsa 0. */
export function stagger(index: number, step = 0.03): number {
  return reducedMotion() ? 0 : index * step;
}

/** Yalnızca transform/opacity: azaltılmış harekette transform kaldırılır, opacity kalır. */
export function enterFrom(offsetY: number): { opacity: number; y?: number } {
  return reducedMotion() ? { opacity: 0 } : { opacity: 0, y: offsetY };
}
