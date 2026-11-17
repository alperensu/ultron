import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { describeBridgeError, type Approval, type ApprovalStatus } from "../../lib/bridge";
import { useStore } from "../../state/store";
import "./approvals.css";

interface ApprovalState {
  pending: Approval[];
  loading: boolean;
  error: string | null;
  feedback: string;
  busy: string | null;
  connected: boolean;
  refresh(clearActionError?: boolean): Promise<void>;
  resolve(id: string, approve: boolean): Promise<void>;
}
const Context = createContext<ApprovalState | null>(null);
export const resolutionLabel = (status: ApprovalStatus) => status === "approved" ? "Onaylandı." : status === "denied" ? "Reddedildi." : status === "expired" ? "Onayın süresi doldu." : "Onay bekliyor.";

/** Olaylar yalnız uyandırır; yetkili ve sayfalı servis yanıtı durumun tek kaynağıdır. */
export function ApprovalProvider({ children }: { children: ReactNode }) {
  const { bridge } = useStore();
  const [pending, setPending] = useState<Approval[]>([]);
  const [loading, setLoading] = useState(true);
  const [connected, setConnected] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [feedback, setFeedback] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const live = useRef(false);
  const resolving = useRef(false);
  const request = useRef<Promise<void> | null>(null);
  const dirty = useRef(false);
  const revision = useRef(0);

  const refresh = useCallback((clearActionError = false): Promise<void> => {
    if (clearActionError) setActionError(null);
    dirty.current = true;
    if (request.current) return request.current;
    const run = async () => {
      while (dirty.current && live.current) {
        dirty.current = false;
        const startedAt = revision.current;
        try {
          const found: Approval[] = [];
          let cursor: string | undefined;
          const cursors = new Set<string>();
          do {
            const page = await bridge.listApprovals({ status: "pending", limit: 32, ...(cursor ? { cursor } : {}) });
            found.push(...page.approvals);
            cursor = page.next_cursor ?? undefined;
            if (cursor && cursors.has(cursor)) throw new Error("Onay sayfalaması ilerlemiyor.");
            if (cursor) cursors.add(cursor);
          } while (cursor && found.length <= 128);
          if (live.current && startedAt === revision.current) {
            setPending([...new Map(found.map((item) => [item.id, item])).values()]);
            setError(null);
            setConnected(true);
          } else if (live.current && dirty.current) continue;
        } catch (err) {
          if (live.current && startedAt === revision.current) {
            setError(describeBridgeError(err));
            setConnected(false);
          }
        } finally {
          if (live.current) setLoading(false);
        }
      }
    };
    request.current = run().finally(() => { request.current = null; });
    return request.current;
  }, [bridge]);

  useEffect(() => {
    let disposed = false;
    const invalidate = () => { revision.current++; };
    live.current = true;
    const off: Array<() => void> = [];
    const track = (subscription: Promise<() => void>) => {
      void subscription.then((unsubscribe) => !disposed ? off.push(unsubscribe) : unsubscribe()).catch((err: unknown) => {
        if (!disposed) setError(describeBridgeError(err));
      });
    };
    track(bridge.onEvent((event) => {
      if (disposed) return;
      if (event.kind !== "approval_requested" && event.kind !== "approval_resolved") return;
      revision.current++;
      if (event.kind === "approval_resolved" && typeof event.payload === "object" && event.payload !== null) {
        const payload = event.payload as { approval_id?: string; status?: ApprovalStatus };
        setPending((items) => items.filter((item) => item.id !== payload.approval_id));
        if (payload.status) setFeedback(resolutionLabel(payload.status));
      }
      void refresh();
    }));
    track(bridge.onDaemonState((state) => {
      if (disposed) return;
      revision.current++;
      if (state.state === "ready") void refresh();
      else {
        setConnected(false);
        setError(describeBridgeError(state.error));
      }
    }));
    void refresh();
    const timer = setInterval(() => { void refresh(); }, 10_000);
    const focus = () => { void refresh(); };
    window.addEventListener("focus", focus);
    return () => {
      disposed = true;
      live.current = false;
      invalidate();
      clearInterval(timer);
      window.removeEventListener("focus", focus);
      off.forEach((unsubscribe) => unsubscribe());
    };
  }, [bridge, refresh]);

  const resolve = useCallback(async (id: string, approve: boolean) => {
    if (resolving.current || !connected) return;
    resolving.current = true;
    setBusy(id);
    setActionError(null);
    try {
      const { approval } = await bridge.resolveApproval(id, approve);
      if (live.current) {
        revision.current++;
        setPending((items) => items.filter((item) => item.id !== id));
        setFeedback(resolutionLabel(approval.status));
      }
    } catch (err) {
      if (live.current) setActionError(describeBridgeError(err));
    } finally {
      resolving.current = false;
      if (live.current) setBusy(null);
      await refresh();
    }
  }, [bridge, connected, refresh]);

  const value = useMemo(() => ({ pending, loading, error: actionError ?? error, feedback, busy, connected, refresh, resolve }), [pending, loading, error, actionError, feedback, busy, connected, refresh, resolve]);
  return <Context.Provider value={value}>{children}</Context.Provider>;
}

export function useApprovals(): ApprovalState {
  const value = useContext(Context);
  if (!value) throw new Error("ApprovalProvider eksik");
  return value;
}
