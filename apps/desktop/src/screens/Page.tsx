import { useState, type ReactNode } from "react";
import { useAppState } from "../state/store";
import { NavBar, QuotaRing } from "../ui";
import { WindowControls } from "../ui/WindowControls";
import { tightestWindow } from "./quota";
import "./screens.css";

/** Kaydırma bu kadar piksele varınca büyük başlık tamamen çubuğa taşınmış sayılır. */
const COLLAPSE_PX = 56;

/**
 * Ortak sayfa düzeni: yarı saydam üst çubuk (küçük başlık kaydıkça belirir, sağda kota halkası),
 * kayan içerik ve isteğe bağlı yapışkan alt bar (`dock`). Büyük başlık içeriğin parçasıdır.
 */
export function Page({
  title,
  dock,
  children,
}: {
  /** Üst çubukta, büyük başlık kaybolunca görünen başlık. */
  title: string;
  dock?: ReactNode;
  children: (progress: number) => ReactNode;
}) {
  const [progress, setProgress] = useState(0);
  return (
    <main className="ui-main">
      <NavBar title={title} progress={progress} tools={<NavQuota />} controls={<WindowControls />} />
      <div className="page">
        <div
          className={`page__scroll${dock ? " has-dock" : ""}`}
          onScroll={(e) => setProgress(Math.min(1, e.currentTarget.scrollTop / COLLAPSE_PX))}
        >
          <div className="page__inner">{children(progress)}</div>
        </div>
        {dock ? <div className="page__dock">{dock}</div> : null}
      </div>
    </main>
  );
}

/** Sağ üstteki kota halkası: en sıkışık bilinen pencerenin doluluğu. Veri yoksa hiçbir şey çizilmez. */
function NavQuota() {
  const quota = useAppState((s) => s.quota);
  const worst = quota?.pools
    .map((pool) => ({ pool, window: tightestWindow(pool) }))
    .filter((x) => x.window?.used_pct != null)
    .sort((a, b) => b.window!.used_pct! - a.window!.used_pct!)[0];
  if (!worst?.window || worst.window.used_pct == null) return null;
  return (
    <span className="navbar-quota">
      <QuotaRing percent={worst.window.used_pct} label={`${poolName(worst.pool.pool)} ${worst.window.label}`} />
    </span>
  );
}

export function poolName(pool: "claude" | "codex" | "agy"): string {
  return pool === "claude" ? "Claude" : pool === "codex" ? "Codex" : "Antigravity";
}
