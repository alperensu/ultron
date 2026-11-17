import { useEffect, useId, useState } from "react";
import { motion } from "motion/react";
import { enterFrom, spring } from "../../design/motion";
import type { Approval } from "../../lib/bridge";
import { ENGINE_LABEL } from "../../lib/format";
import { chatPath, navigate } from "../../state/router";
import { CapsuleButton } from "../../ui";
import { riskLabel } from "./labels";

/** Yalnız servisin maskelediği action_json okunur; HTML veya komut yürütülmez. */
export function actionSummary(json: string): string | null {
  try {
    const value = JSON.parse(json) as Record<string, Record<string, unknown>>;
    for (const key of ["Shell", "FileWrite", "FileDelete", "Network", "ToolCall"]) {
      const action = value[key];
      if (action && typeof action === "object") {
        if (key === "ToolCall") {
          const input = action.input;
          if (action.tool === "ultron.turn.elevate" && typeof input === "object" && input !== null) {
            const preview = (input as Record<string, unknown>).text_preview;
            return `Bu tur için tam araç politikası${typeof preview === "string" ? `\n${preview}` : ""}`;
          }
          return `Araç: ${typeof action.tool === "string" ? action.tool : "Bilinmiyor"}\n${JSON.stringify(input ?? {}, null, 2)}`;
        }
        const summary = action.command ?? action.path ?? action.url ?? action.tool;
        if (typeof summary === "string") return key === "Shell" && typeof action.cwd === "string" ? `${summary}\nÇalışma dizini: ${action.cwd}` : summary;
      }
    }
  } catch { /* Bozuk özeti komut gibi yorumlama. */ }
  return null;
}

export function ApprovalCard({ approval, disabled = false, busy = false, onResolve }: {
  approval: Approval;
  disabled?: boolean;
  busy?: boolean;
  onResolve: (id: string, approve: boolean) => void;
}) {
  const title = useId();
  const reason = useId();
  const summaryId = useId();
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  // IPC kesin deadline taşımıyor. 300 sn servis üst sınırıdır; kısa istekler erken bitebilir.
  const remaining = Math.max(0, Math.ceil((Date.parse(approval.created_at) + 300_000 - now) / 1000));
  const expired = !Number.isFinite(remaining) || remaining === 0;
  const summary = actionSummary(approval.action_json);
  return (
    <motion.article className="approval-card" aria-labelledby={title} aria-describedby={reason} aria-busy={busy}
      initial={enterFrom(12)} animate={{ opacity: 1, y: 0 }} exit={enterFrom(-12)} transition={spring()}>
      <div className="approval-card__head">
        <h3 id={title}>{riskLabel(approval.category)}</h3>
        <span>{ENGINE_LABEL[approval.engine]}</span>
      </div>
      <p id={reason}>{approval.reason}</p>
      <pre id={summaryId} className="approval-card__summary" tabIndex={0} aria-label="Maskeli işlem özeti">{summary ?? "İşlem özeti okunamadı. Karar verilemiyor; onayları yeniden yükle."}</pre>
      <div className="approval-card__meta">
        <span aria-label="Kalan süre üst sınırı" title="Servisin azami bekleme süresi. İstek daha erken sona erebilir.">
          {expired ? "Süre doldu · servis kararı bekleniyor" : `En fazla ${Math.floor(remaining / 60)}:${String(remaining % 60).padStart(2, "0")} kaldı`}
        </span>
        {approval.chat_id ? <button type="button" onClick={() => navigate(chatPath(approval.chat_id!))}>Sohbete git</button> : <span>Genel işlem</span>}
      </div>
      <div className="approval-actions">
        <CapsuleButton aria-describedby={`${title} ${reason} ${summaryId}`} disabled={disabled || expired || busy || !summary} onClick={() => onResolve(approval.id, true)}>Onayla</CapsuleButton>
        <CapsuleButton aria-describedby={`${title} ${reason} ${summaryId}`} variant="danger" disabled={disabled || expired || busy} onClick={() => onResolve(approval.id, false)}>Reddet</CapsuleButton>
        {busy ? <span role="status">Karar iletiliyor…</span> : null}
      </div>
    </motion.article>
  );
}
