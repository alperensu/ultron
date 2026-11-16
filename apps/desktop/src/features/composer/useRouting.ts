import { useCallback, useEffect, useRef, useState } from "react";
import type { Bridge, RouteResult } from "../../lib/bridge";

export const ROUTE_DEBOUNCE_MS = 300;
/** Bu güvenin altında ve alternatif varsa gönderimden önce sorulur. */
export const ROUTE_CONFIDENCE_FLOOR = 0.6;

export function isAmbiguous(route: RouteResult | null): boolean {
  return route !== null && route.confidence < ROUTE_CONFIDENCE_FLOOR && route.alternatives.length > 0;
}

/**
 * Yazılan metni 300 ms gecikmeyle `routeMessage` ile yönlendirir. Yönlendirme başarısız olursa
 * (servis yok, henüz uygulanmamış) sonuç null kalır ve kullanıcı projeyi elle seçer.
 * `resolve`, gönderim anında güncel sonucu garanti eder (gecikme dolmadan gönderilirse hemen sorar).
 */
export function useRouting(bridge: Bridge, text: string) {
  const [route, setRoute] = useState<RouteResult | null>(null);
  const settled = useRef<{ text: string; route: RouteResult | null } | null>(null);
  const trimmed = text.trim();

  useEffect(() => {
    if (trimmed === "") {
      settled.current = null;
      setRoute(null);
      return;
    }
    let cancelled = false;
    const timer = window.setTimeout(() => {
      bridge.routeMessage(trimmed).then(
        (result) => {
          if (cancelled) return;
          settled.current = { text: trimmed, route: result };
          setRoute(result);
        },
        () => {
          if (cancelled) return;
          settled.current = { text: trimmed, route: null };
          setRoute(null);
        },
      );
    }, ROUTE_DEBOUNCE_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [bridge, trimmed]);

  const resolve = useCallback(async (): Promise<RouteResult | null> => {
    if (settled.current?.text === trimmed) return settled.current.route;
    try {
      return await bridge.routeMessage(trimmed);
    } catch {
      return null;
    }
  }, [bridge, trimmed]);

  return { route, resolve };
}
