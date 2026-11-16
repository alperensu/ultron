import type { ReactNode } from "react";

export function Chip({ children, tone }: { children: ReactNode; tone?: "blue" | "green" | "orange" | "red" }) {
  return <span className={`ui-chip${tone ? ` tone-${tone}` : ""}`}>{children}</span>;
}
