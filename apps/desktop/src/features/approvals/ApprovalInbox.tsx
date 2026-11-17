import { useRef, useState } from "react";
import { AnimatePresence } from "motion/react";
import { CapsuleButton, EmptyState, GroupedList } from "../../ui";
import { ApprovalCard } from "./ApprovalCard";
import { useApprovals } from "./ApprovalProvider";

/** Küçük sayfalar DOM'u ve klavye gezinmesini sınırlı tutar. */
const PAGE_SIZE = 6;
export function ApprovalInbox({ chatId }: { chatId?: string }) {
  const state = useApprovals();
  const [requestedPage, setPage] = useState(0);
  const container = useRef<HTMLDivElement>(null);
  const items = chatId ? state.pending.filter((item) => item.chat_id === chatId) : state.pending;
  const page = Math.min(requestedPage, Math.max(0, Math.ceil(items.length / PAGE_SIZE) - 1));
  return (
    <GroupedList header={chatId ? "Bu sohbette onay bekleyen" : "Seni bekleyen"}>
      <div className="approval-inbox" ref={container} tabIndex={-1} aria-label="Onay kutusu">
        {state.loading ? <p role="status">Onaylar okunuyor…</p> : null}
        {state.error ? <div role="alert"><p>{state.error}</p><CapsuleButton variant="text" onClick={() => void state.refresh(true)}>Onayları yeniden yükle</CapsuleButton></div> : null}
        <p className="approval-feedback" role="status" aria-live="polite">{state.feedback}</p>
        {!state.loading && !state.error && items.length === 0 ? <EmptyState title="Bekleyen onay yok" description="Riskli bir işlem onayını beklediğinde burada görünür." /> : null}
        <AnimatePresence initial={false}>
          {items.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE).map((approval) => (
            <ApprovalCard key={approval.id} approval={approval} disabled={!state.connected || state.busy !== null} busy={state.busy === approval.id}
              onResolve={(id, approve) => {
                const focused = document.activeElement;
                void state.resolve(id, approve).then(() => {
                  if (document.activeElement === focused || document.activeElement === document.body) container.current?.focus();
                });
              }} />
          ))}
        </AnimatePresence>
        {items.length > PAGE_SIZE ? <nav className="approval-actions" aria-label="Onay sayfaları">
          <CapsuleButton variant="text" disabled={page === 0} onClick={() => setPage(page - 1)}>Önceki</CapsuleButton>
          <span>{page + 1} / {Math.ceil(items.length / PAGE_SIZE)}</span>
          <CapsuleButton variant="text" disabled={(page + 1) * PAGE_SIZE >= items.length} onClick={() => setPage(page + 1)}>Sonraki</CapsuleButton>
        </nav> : null}
      </div>
    </GroupedList>
  );
}
