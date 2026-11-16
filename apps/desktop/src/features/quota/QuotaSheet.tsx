import { useEffect, useId, useRef, useState, type FormEvent } from "react";
import { createPortal } from "react-dom";
import { describeBridgeError, type PoolReport, type QuotaSource, type QuotaWindow } from "../../lib/bridge";
import { ENGINE_LABEL } from "../../lib/format";
import { useStore } from "../../state/store";
import { CapsuleButton } from "../../ui/CapsuleButton";
import { GroupedList } from "../../ui/GroupedList";
import { quotaTone } from "../../ui/QuotaRing";
import { Row } from "../../ui/Row";
import { Sheet } from "../../ui/Sheet";
import { useQuota } from "./QuotaProvider";

const SOURCE_LABEL: Record<QuotaSource, string> = {
  Live: "Canlı",
  Estimated: "Tahmini",
  Unknown: "Bilinmiyor",
};

export function resetCountdown(resetsAt: string | null, now = Date.now()): string {
  if (!resetsAt || !Number.isFinite(Date.parse(resetsAt))) return "Sıfırlanma zamanı bilinmiyor";
  const minutes = Math.ceil(Math.max(0, Date.parse(resetsAt) - now) / 60_000);
  if (minutes === 0) return "Sıfırlanma zamanı geldi";
  if (minutes < 60) return `sıfırlanma ${minutes} dk`;
  return `sıfırlanma ${Math.floor(minutes / 60)} sa ${minutes % 60} dk`;
}

function modeLabel(mode: string): string {
  switch (mode.toLowerCase()) {
    case "tam": return "Tam";
    case "dengeli": return "Dengeli";
    case "eko": return "Eko";
    default: return "Mod bilinmiyor";
  }
}

export function QuotaSheet({ open, onClose }: { open: boolean; onClose(): void }) {
  const quota = useQuota();
  if (!quota) return null;
  // NavBar'ın backdrop-filter'ı sabit konumlu öğeleri hapseder; panel body'ye taşınır.
  return createPortal(
    <Sheet open={open} onClose={onClose} title="Kota" side="right">
      <div className="quota-sheet__intro">
        <p>Motor başına kullanım pencereleri</p>
        <CapsuleButton variant="text" onClick={onClose}>Bitti</CapsuleButton>
      </div>
      <div className="quota-sheet__refresh">
        <span role="status">{quota.refreshing ? "Kota yenileniyor…" : "Her 60 saniyede yenilenir"}</span>
        <CapsuleButton variant="text" disabled={quota.refreshing} onClick={() => void quota.refresh()}>
          Yenile
        </CapsuleButton>
      </div>
      {quota.error ? <p className="quota-error" role="alert">Kota okunamadı: {quota.error}</p> : null}
      {!quota.report && !quota.error ? <p className="quota-note">Kota bilgisi bekleniyor.</p> : null}
      {quota.report && (quota.report.pools ?? []).length === 0 ? <p className="quota-note">Kota penceresi bulunamadı.</p> : null}
      {(quota.report?.pools ?? []).map((pool) => (
        <GroupedList key={pool.pool} header={ENGINE_LABEL[pool.pool]}>
          <Row title="Bütçe modu" trailing={<span className="ui-chip">{modeLabel(pool.mode)}</span>} />
          {(pool.windows ?? []).length === 0 ? <Row title="Pencere yok" meta="Bilinmiyor" /> : null}
          {(pool.windows ?? []).map((window, index) => (
            <WindowUsage key={`${window.label}-${index}`} window={window} pool={pool.pool} />
          ))}
          {pool.pool !== "agy" ? <ResetRights pool={pool} /> : null}
          {pool.note ? <p className="quota-note">{pool.note}</p> : null}
        </GroupedList>
      ))}
    </Sheet>,
    document.body,
  );
}

function WindowUsage({ window, pool }: { window: QuotaWindow; pool: PoolReport["pool"] }) {
  const percent = window.used_pct !== null && Number.isFinite(window.used_pct)
    ? Math.max(0, Math.min(100, window.used_pct))
    : null;
  return (
    <div className="quota-window">
      <div className="quota-window__heading">
        <span>{window.label}</span>
        <strong>{percent === null ? "Bilinmiyor" : `%${Math.round(percent)}`}</strong>
        <span className="ui-chip">{SOURCE_LABEL[window.source]}</span>
      </div>
      <div
        className="quota-window__bar"
        role="progressbar"
        aria-label={`${ENGINE_LABEL[pool]} ${window.label}`}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={percent ?? undefined}
        aria-valuetext={percent === null ? "Bilinmiyor" : `%${Math.round(percent)}`}
        data-tone={percent === null ? "unknown" : quotaTone(percent)}
      >
        <span style={{ width: `${percent ?? 0}%` }} />
      </div>
      <p className="quota-window__reset">{resetCountdown(window.resets_at)}</p>
    </div>
  );
}

function ResetRights({ pool }: { pool: PoolReport }) {
  const { bridge } = useStore();
  const quota = useQuota();
  const id = useId();
  const [count, setCount] = useState(pool.reset_rights?.toString() ?? "");
  const [savedCount, setSavedCount] = useState(pool.reset_rights);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const busy = useRef(false);

  useEffect(() => {
    setSavedCount(pool.reset_rights);
    setCount(pool.reset_rights?.toString() ?? "");
  }, [pool.reset_rights]);

  async function save(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (busy.current) return;
    const value = Number(count);
    if (!/^\d+$/.test(count) || !Number.isInteger(value) || value < 0 || value > 4_294_967_295) {
      setError("0 ile 4294967295 arasında bir tam sayı gir.");
      return;
    }
    busy.current = true;
    setSaving(true);
    setError(null);
    setMessage(null);
    try {
      await bridge.setResetRights(pool.pool, value);
      setSavedCount(value);
      setMessage("Sıfırlama hakkı kaydedildi.");
      await quota?.refresh(true);
    } catch (err) {
      setError(describeBridgeError(err));
    } finally {
      busy.current = false;
      setSaving(false);
    }
  }

  return (
    <form className="quota-rights" onSubmit={(event) => void save(event)}>
      <label htmlFor={id}>Sıfırlama hakkı</label>
      <div className="quota-rights__controls">
        <input
          id={id}
          aria-label={`${ENGINE_LABEL[pool.pool]} sıfırlama hakkı`}
          className="quota-input"
          type="number"
          min={0}
          max={4_294_967_295}
          step={1}
          placeholder="Bilinmiyor"
          value={count}
          disabled={saving}
          onChange={(event) => {
            setCount(event.target.value);
            setMessage(null);
            setError(null);
          }}
        />
        <CapsuleButton type="submit" disabled={saving || count === "" || Number(count) === savedCount}>
          {saving ? "Kaydediliyor…" : "Kaydet"}
        </CapsuleButton>
      </div>
      {error ? <p className="quota-error" role="alert">{error}</p> : null}
      {message ? <p className="quota-note" role="status">{message}</p> : null}
    </form>
  );
}
