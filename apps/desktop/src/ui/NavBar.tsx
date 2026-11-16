import type { CSSProperties, ReactNode } from "react";
import { NavQuota } from "../features/quota/NavQuota";

/**
 * Üst çubuk. Tauri penceresinde özel başlık alanıdır: boş yüzeyi sürükleme bölgesidir
 * (`data-tauri-drag-region`); araçlar ve pencere düğmeleri sürüklemeyi tutmaz.
 * `progress` 0..1: büyük başlık kaydıkça küçük başlığın görünürlüğü.
 */
export function NavBar({
  title,
  progress = 1,
  tools,
  controls,
}: {
  title?: string;
  progress?: number;
  tools?: ReactNode;
  controls?: ReactNode;
}) {
  const style = { "--p": progress } as CSSProperties;
  return (
    <header className="ui-navbar" data-tauri-drag-region style={style}>
      <span data-tauri-drag-region />
      <div className="ui-navbar__title" data-tauri-drag-region>
        {title}
      </div>
      <div className="ui-navbar__tools">
        {tools}
        <NavQuota />
        {controls}
      </div>
    </header>
  );
}
