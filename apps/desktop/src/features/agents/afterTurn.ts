import { isBridgeError } from "../../lib/bridge";

/** Tamamlanma/iptal bildirimi, sunucudaki tur kilidinin bırakılmasından önce gelebilir.
 * Yalnız kabul edilmediği kesin olan busy reddi yeniden denenir; taşıma hatası denenmez.
 */
export async function afterTurn<T>(action: () => Promise<T>): Promise<T> {
  for (let attempt = 0; ; attempt++) {
    try { return await action(); }
    catch (error) {
      if (attempt >= 39 || !isBridgeError(error) || error.kind !== "daemon" || error.message !== "sohbette çalışan bir tur var") throw error;
      await new Promise<void>((resolve) => window.setTimeout(resolve, 125));
    }
  }
}
