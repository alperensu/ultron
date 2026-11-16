import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import { describeBridgeError, type QuotaReport } from "../../lib/bridge";
import { useAppState, useStore } from "../../state/store";
import "./quota.css";

interface QuotaState {
  report: QuotaReport | null;
  error: string | null;
  refreshing: boolean;
  /** Mutasyondan sonra önceki okumayı bekleyip yeni bir okuma başlatır. */
  refresh(afterPending?: boolean): Promise<void>;
}

const QuotaContext = createContext<QuotaState | null>(null);

/** Paylaşılan halka/panel verisi; açılışta ve 60 saniyede bir tazelenir. */
export function QuotaProvider({ children }: { children: ReactNode }) {
  const { bridge } = useStore();
  const initialReport = useAppState((state) => state.quota);
  const initialError = useAppState((state) => state.quotaError);
  const [result, setResult] = useState<{ report: QuotaReport | null; error: string | null } | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const pending = useRef<Promise<void> | null>(null);
  const mounted = useRef(false);

  const refresh = useCallback((afterPending = false): Promise<void> => {
    if (!mounted.current) return Promise.resolve();
    if (pending.current) return afterPending ? pending.current.then(() => refresh()) : pending.current;
    setRefreshing(true);
    const request = bridge.getQuota().then(
      (report) => {
        if (mounted.current) setResult({ report, error: null });
      },
      (error: unknown) => {
        if (mounted.current) setResult({ report: null, error: describeBridgeError(error) });
      },
    ).finally(() => {
      pending.current = null;
      if (mounted.current) setRefreshing(false);
    });
    pending.current = request;
    return request;
  }, [bridge]);

  useEffect(() => {
    mounted.current = true;
    const timer = window.setInterval(() => void refresh(), 60_000);
    return () => {
      mounted.current = false;
      window.clearInterval(timer);
    };
  }, [refresh]);

  return (
    <QuotaContext.Provider value={{
      report: result ? result.report : initialReport,
      error: result ? result.error : initialError,
      refreshing,
      refresh,
    }}>
      {children}
    </QuotaContext.Provider>
  );
}

/** UI kataloğunun bağımsız NavBar örneklerinde sağlayıcı bulunmayabilir. */
export function useQuota(): QuotaState | null {
  return useContext(QuotaContext);
}
