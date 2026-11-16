import { getCurrentWindow } from "@tauri-apps/api/window";

/** Tauri başlık çubuğu düğmeleri. Tarayıcıda (Tauri yok) hiçbir şey çizmez. */
export function WindowControls() {
  if (!("__TAURI_INTERNALS__" in window)) return null;
  const win = getCurrentWindow();
  return (
    <div className="ui-winctl">
      <button type="button" aria-label="Küçült" onClick={() => void win.minimize()}>
        <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M0 5h10" /></svg>
      </button>
      <button type="button" aria-label="Büyüt veya geri yükle" onClick={() => void win.toggleMaximize()}>
        <svg viewBox="0 0 10 10" aria-hidden="true"><rect x="0.5" y="0.5" width="9" height="9" /></svg>
      </button>
      <button type="button" className="close" aria-label="Kapat" onClick={() => void win.close()}>
        <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M0 0l10 10M10 0L0 10" /></svg>
      </button>
    </div>
  );
}
