import { useEffect, useRef, useState, type FormEvent } from "react";
import { describeBridgeError, type PolicySettings as Settings } from "../../lib/bridge";
import { useAppState, useStore } from "../../state/store";
import { CapsuleButton, GroupedList } from "../../ui";
import "./approvals.css";

export function PolicySettings() {
  const { bridge } = useStore();
  const daemon = useAppState((state) => state.daemon.state);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [finance, setFinance] = useState("");
  const [protectedPaths, setProtected] = useState("");
  const [retention, setRetention] = useState("30");
  const [error, setError] = useState<string | null>(null);
  const [feedback, setFeedback] = useState("");
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [reload, setReload] = useState(0);
  const lock = useRef(false);
  const mounted = useRef(false);
  const generation = useRef(0);
  useEffect(() => {
    const invalidate = () => { generation.current++; };
    mounted.current = true;
    return () => { mounted.current = false; invalidate(); };
  }, []);
  useEffect(() => {
    const current = ++generation.current;
    setLoading(true);
    setSettings(null);
    setFeedback("");
    void bridge.getPolicyConfig().then((value) => {
      if (!mounted.current || current !== generation.current) return;
      setSettings(value);
      setFinance(value.config.finance_paths.join("\n"));
      setProtected(value.config.protected_paths.join("\n"));
      setRetention(String(value.trash_retention_days));
      setError(null);
    }, (err: unknown) => {
      if (mounted.current && current === generation.current) setError(describeBridgeError(err));
    }).finally(() => { if (mounted.current && current === generation.current) setLoading(false); });
  }, [bridge, daemon, reload]);

  async function save(event: FormEvent) {
    event.preventDefault();
    if (!settings || lock.current) return;
    lock.current = true;
    setSaving(true);
    setError(null);
    setFeedback("");
    const current = generation.current;
    const lines = (text: string) => text.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
    try {
      const updated = await bridge.setPolicyConfig({
        config: { ...settings.config, finance_paths: lines(finance), protected_paths: lines(protectedPaths) },
        trash_retention_days: Number(retention),
      });
      if (mounted.current && current === generation.current) {
        setSettings(updated);
        setFinance(updated.config.finance_paths.join("\n"));
        setProtected(updated.config.protected_paths.join("\n"));
        setRetention(String(updated.trash_retention_days));
        setFeedback("Kurallar kaydedildi.");
      }
    } catch (err) {
      if (mounted.current && current === generation.current) setError(describeBridgeError(err));
    } finally {
      lock.current = false;
      if (mounted.current) setSaving(false);
    }
  }
  return (
    <GroupedList header="Kurallar" footer="Yerleşik güvenlik kuralları kapatılamaz. Ayarlar yalnız onaylayıcı bağlantısı üzerinden policy.toml dosyasına yazılır.">
      <form className="policy-form" onSubmit={(event) => void save(event)} aria-busy={loading || saving}>
        <p className="policy-help">Silme, Git geçmişi, dağıtım, finans, gizli bilgi, kurulum, sistem ayarı ve yayın işlemleri denetlenir. İzin atlama ve tehlikeli disk işlemleri reddedilir.</p>
        {loading ? <p role="status">Kurallar okunuyor…</p> : null}
        {error ? <div role="alert"><p>{error}</p>{!settings ? <CapsuleButton variant="text" onClick={() => setReload((value) => value + 1)}>Kuralları yeniden yükle</CapsuleButton> : null}</div> : null}
        {settings ? <>
          <label>Finans yolları<textarea value={finance} onChange={(event) => setFinance(event.target.value)} disabled={saving} spellCheck={false} autoComplete="off" /><small>Her satıra bir mutlak klasör yolu.</small></label>
          <label>Korumalı yollar<textarea value={protectedPaths} onChange={(event) => setProtected(event.target.value)} disabled={saving} spellCheck={false} autoComplete="off" /><small>Bu yollar altındaki yazma işlemleri onay ister.</small></label>
          <label>Çöp saklama süresi (gün)<input type="number" min={1} max={3650} step={1} required value={retention} disabled={saving} onChange={(event) => setRetention(event.target.value)} /></label>
          <details><summary>Diğer etkin kullanıcı kuralları (salt okunur)</summary>
            <p>Dağıtım desenleri</p><pre className="approval-card__summary">{settings.config.deploy_patterns.join("\n") || "Ek desen yok."}</pre>
            <p>İzin verilen tam komutlar</p><pre className="approval-card__summary">{settings.config.allow_list.join("\n") || "Ek izin yok."}</pre>
          </details>
          <CapsuleButton type="submit" disabled={saving || loading}>{saving ? "Kaydediliyor…" : "Kuralları kaydet"}</CapsuleButton>
        </> : null}
        <p role="status">{feedback}</p>
      </form>
    </GroupedList>
  );
}
