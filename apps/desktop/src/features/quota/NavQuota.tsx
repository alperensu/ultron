import { useCallback, useState } from "react";
import { ENGINE_LABEL } from "../../lib/format";
import { QuotaRing } from "../../ui/QuotaRing";
import { useQuota } from "./QuotaProvider";
import { QuotaSheet } from "./QuotaSheet";

/** En dolu bilinen pencere; ölçüm yoksa yüzde uydurmadan panel erişimini korur. */
export function NavQuota() {
  const quota = useQuota();
  const [open, setOpen] = useState(false);
  const close = useCallback(() => setOpen(false), []);
  if (!quota) return null;
  const tightest = (quota.report?.pools ?? []).flatMap((pool) => (pool.windows ?? []).map((window) => ({ pool, window })))
    .filter(({ window }) => window.used_pct !== null && Number.isFinite(window.used_pct))
    .sort((a, b) => b.window.used_pct! - a.window.used_pct!)[0];

  return (
    <>
      <button
        type="button"
        className="navbar-quota quota-trigger press"
        aria-label="Kota panelini aç"
        aria-haspopup="dialog"
        aria-expanded={open}
        onClick={() => {
          setOpen(true);
          void quota.refresh();
        }}
      >
        {tightest ? (
          <QuotaRing
            percent={tightest.window.used_pct!}
            label={`${ENGINE_LABEL[tightest.pool.pool]} ${tightest.window.label}`}
          />
        ) : <span className="quota-trigger__unknown" aria-label="Kota bilinmiyor">?</span>}
      </button>
      <QuotaSheet open={open} onClose={close} />
    </>
  );
}
