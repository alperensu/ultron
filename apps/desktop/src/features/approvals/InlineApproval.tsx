import { AnimatePresence, motion } from "motion/react";
import { enterFrom, spring } from "../../design/motion";
import type { ApprovalStatus } from "../../lib/bridge";
import { ApprovalCard } from "./ApprovalCard";
import { resolutionLabel, useApprovals } from "./ApprovalProvider";
import { useRef } from "react";
import { CapsuleButton } from "../../ui";

export function InlineApproval({ approvalId, status }: { approvalId: string; status: ApprovalStatus }) {
  const state = useApprovals();
  const ref = useRef<HTMLDivElement>(null);
  const approval = state.pending.find((item) => item.id === approvalId);
  return <div className="approval-inline" ref={ref} tabIndex={-1}>
    <AnimatePresence mode="wait" initial={false}>
      {status === "pending" && approval ? <ApprovalCard key={approval.id} approval={approval} busy={state.busy === approval.id}
        disabled={!state.connected || state.busy !== null} onResolve={(id, approve) => {
          const focused = document.activeElement;
          void state.resolve(id, approve).then(() => {
            if (document.activeElement === focused || document.activeElement === document.body) ref.current?.focus();
          });
        }} /> :
        <motion.p key={status} role="status" initial={enterFrom(6)} animate={{ opacity: 1, y: 0 }} exit={enterFrom(-6)} transition={spring()}>
          {status !== "pending" ? resolutionLabel(status) : state.loading ? "Onay bilgisi okunuyor…" : "Güncel onay durumu için Bugün ekranındaki kutuyu kontrol et."}
        </motion.p>}
    </AnimatePresence>
    {state.error && status === "pending" ? <div role="alert"><p>{state.error}</p><CapsuleButton variant="text" onClick={() => void state.refresh(true)}>Onayları yeniden yükle</CapsuleButton></div> : null}
  </div>;
}
