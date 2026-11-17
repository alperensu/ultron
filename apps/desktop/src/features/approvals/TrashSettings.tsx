import { useCallback, useEffect, useRef, useState } from "react";
import { describeBridgeError, type TrashEntry } from "../../lib/bridge";
import { useAppState, useStore } from "../../state/store";
import { CapsuleButton, EmptyState, GroupedList, Sheet } from "../../ui";
import "./approvals.css";

const PAGE_SIZE = 10;
export function TrashSettings() {
  const { bridge } = useStore();
  const daemon = useAppState((state) => state.daemon.state);
  const [entries, setEntries] = useState<TrashEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [feedback, setFeedback] = useState("");
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<TrashEntry | null>(null);
  const [requestedPage, setPage] = useState(0);
  const live = useRef(false);
  const lock = useRef(false);
  const revision = useRef(0);
  const list = useRef<HTMLDivElement>(null);
  const refresh = useCallback(async () => {
    const current = ++revision.current;
    setLoading(true);
    try {
      const result = await bridge.listTrash();
      if (live.current && current === revision.current) { setEntries(result.entries); setError(null); }
    } catch (err) {
      if (live.current && current === revision.current) setError(describeBridgeError(err));
    } finally { if (live.current && current === revision.current) setLoading(false); }
  }, [bridge]);
  useEffect(() => {
    const invalidate = () => { revision.current++; };
    live.current = true;
    void refresh();
    const focus = () => { if (!lock.current) void refresh(); };
    window.addEventListener("focus", focus);
    return () => { live.current = false; invalidate(); window.removeEventListener("focus", focus); };
  }, [refresh, daemon]);
  const close = useCallback(() => { if (!lock.current) setConfirm(null); }, []);
  async function act(entry: TrashEntry, purge: boolean) {
    if (lock.current) return;
    lock.current = true;
    revision.current++;
    setBusy(entry.id);
    setError(null);
    setFeedback("");
    try {
      const result = purge ? await bridge.purgeTrash(entry.id) : await bridge.restoreTrash(entry.id);
      if (live.current) {
        setEntries((items) => items.filter((item) => item.id !== entry.id));
        setFeedback("path" in result ? `Geri yüklendi: ${result.path}` : "Kayıt kalıcı olarak silindi.");
        setConfirm(null);
        list.current?.focus();
      }
    } catch (err) {
      const message = describeBridgeError(err);
      if (live.current && message.startsWith("sonuç belirsiz")) {
        // Mutasyon asla otomatik tekrarlanmaz; gerçek durumu listeden uzlaştır.
        setFeedback("Sonuç belirsiz: yanıt alınamadı, işlem uygulanmış olabilir. Liste yenilendi; kaydın durumunu kontrol edin.");
        setConfirm(null);
        await refresh();
      } else if (live.current) setError(message);
    } finally {
      lock.current = false;
      if (live.current) setBusy(null);
      // Başarılı mutasyonun sonucunu liste hatasıyla örtme; yenileme kullanıcıya açık.
    }
  }
  const page = Math.min(requestedPage, Math.max(0, Math.ceil(entries.length / PAGE_SIZE) - 1));
  return <GroupedList header="Çöp kutusu" footer="Geri yükleme mevcut dosyanın üzerine yazmaz; gerekirse yeni ad kullanır. Kalıcı silme geri alınamaz.">
    <div className="trash-list" ref={list} tabIndex={-1} aria-label="Çöp kutusu kayıtları" aria-busy={loading || busy !== null}>
      <CapsuleButton variant="text" disabled={loading || busy !== null} onClick={() => void refresh()}>Çöp kutusunu yenile</CapsuleButton>
      {loading ? <p role="status">Çöp kutusu okunuyor…</p> : null}
      {error ? <p role="alert">{error}</p> : null}
      <p role="status" className="trash-entry__path">{feedback}</p>
      {!loading && !error && entries.length === 0 ? <EmptyState title="Çöp kutusu boş" description="Geri alınabilir biçimde silinen dosyalar burada görünür." /> : null}
      {entries.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE).map((entry) => <article key={entry.id} className="trash-entry" aria-label={entry.original_path}>
        <p className="trash-entry__path" id={`trash-path-${entry.id}`}>{entry.original_path}</p>
        <small>{entry.is_dir ? "Klasör" : "Dosya"} · {entry.size.toLocaleString("tr-TR")} bayt · {new Date(entry.deleted_at).toLocaleString("tr-TR")}</small>
        <div className="approval-actions">
          <CapsuleButton aria-describedby={`trash-path-${entry.id}`} disabled={busy !== null || loading} onClick={() => void act(entry, false)}>Geri yükle</CapsuleButton>
          <CapsuleButton aria-describedby={`trash-path-${entry.id}`} variant="danger" disabled={busy !== null || loading} onClick={() => setConfirm(entry)}>Kalıcı sil</CapsuleButton>
        </div>
      </article>)}
      {entries.length > PAGE_SIZE ? <nav className="approval-actions" aria-label="Çöp kutusu sayfaları">
        <CapsuleButton variant="text" disabled={page === 0} onClick={() => setPage(page - 1)}>Önceki</CapsuleButton>
        <span>{page + 1} / {Math.ceil(entries.length / PAGE_SIZE)}</span>
        <CapsuleButton variant="text" disabled={(page + 1) * PAGE_SIZE >= entries.length} onClick={() => setPage(page + 1)}>Sonraki</CapsuleButton>
      </nav> : null}
    </div>
    <Sheet open={confirm !== null} onClose={close} title="Kalıcı silmeyi onayla">
      <div className="trash-list">
        <p className="trash-entry__path">{confirm?.original_path}</p>
        <p>Bu kayıt çöp kutusundan kalıcı olarak silinecek. Bu işlem geri alınamaz.</p>
        {error ? <p role="alert">{error}</p> : null}
        <div className="approval-actions">
          <CapsuleButton variant="text" disabled={busy !== null} onClick={close}>Vazgeç</CapsuleButton>
          <CapsuleButton variant="danger" disabled={busy !== null} onClick={() => { if (confirm) void act(confirm, true); }}>{busy ? "Siliniyor…" : "Evet, kalıcı sil"}</CapsuleButton>
        </div>
      </div>
    </Sheet>
  </GroupedList>;
}
