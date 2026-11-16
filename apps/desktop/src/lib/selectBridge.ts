import { createFakeBridge, createTauriBridge, type Bridge, type FakeBridgeOptions } from "./bridge";

declare global {
  interface Window {
    /** Yalnızca geliştirme/e2e: Playwright sahte köprü verisini buradan verir. Üretim derlemesinde okunmaz. */
    __ULTRON_FAKE_BRIDGE__?: FakeBridgeOptions;
  }
}

/** Tauri içinde gerçek köprü; geliştirme sunucusunda e2e verisi varsa sahte köprü; aksi halde gerçek köprü. */
export function selectBridge(): Bridge {
  if (import.meta.env.DEV && window.__ULTRON_FAKE_BRIDGE__ && !("__TAURI_INTERNALS__" in window)) {
    return createFakeBridge(window.__ULTRON_FAKE_BRIDGE__);
  }
  return createTauriBridge();
}
