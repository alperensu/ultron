import type { PoolReport, QuotaWindow } from "../lib/bridge";

/** Havuzun en sıkışık penceresi: bilinen doluluğu en yüksek olan; hiçbiri bilinmiyorsa ilk pencere. */
export function tightestWindow(pool: PoolReport): QuotaWindow | null {
  const known = pool.windows.filter((w) => w.used_pct !== null);
  if (known.length === 0) return pool.windows[0] ?? null;
  return known.reduce((a, b) => (b.used_pct! > a.used_pct! ? b : a));
}
