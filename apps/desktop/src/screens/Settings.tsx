import { useEffect, useRef, useState, type FormEvent } from "react";
import { describeBridgeError, type CatalogImportReport, type ProjectImportReport } from "../lib/bridge";
import { ENGINE_LABEL } from "../lib/format";
import { useAppState, useStore } from "../state/store";
import { CapsuleButton, GroupedList, LargeTitle, Row } from "../ui";
import { Page } from "./Page";
import "../features/quota/quota.css";
import { PolicySettings } from "../features/approvals/PolicySettings";
import { TrashSettings } from "../features/approvals/TrashSettings";
import { TelegramSettings } from "../features/telegram/TelegramSettings";

/** Servis mutasyonlarıyla desteklenmeyen ayarlar salt okunur gösterilir. */
export function Settings() {
  const { bridge } = useStore();
  const daemon = useAppState((state) => state.daemon);
  const [status, setStatus] = useState<Awaited<ReturnType<typeof bridge.status>> | null>(null);
  const [statusError, setStatusError] = useState<string | null>(null);
  const [path, setPath] = useState("");
  const catalog = useImport<CatalogImportReport>();
  const history = useImport<ProjectImportReport>();

  useEffect(() => {
    let active = true;
    setStatus(null);
    setStatusError(null);
    void bridge.status().then(
      (value) => { if (active) setStatus(value); },
      (error: unknown) => { if (active) setStatusError(describeBridgeError(error)); },
    );
    return () => { active = false; };
  }, [bridge, daemon.state]);

  function importCatalog(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const trimmed = path.trim();
    if (trimmed) void catalog.run(() => bridge.importCatalog(trimmed));
  }

  const connected = daemon.state === "ready" && !statusError;

  return (
    <Page title="Ayarlar">
      {(progress) => (
        <>
          <LargeTitle title="Ayarlar" summary="Servis, motorlar ve içe aktarma" progress={progress} />
          <PolicySettings />
          <TrashSettings />
          <TelegramSettings />
          <GroupedList header="Servis">
            <Row
              title="ultrond"
              subtitle={statusError ?? (daemon.state === "unavailable" ? daemon.reason : undefined)}
              meta={connected ? "Bağlı" : daemon.state === "connecting" ? "Bağlanıyor…" : "Bağlı değil"}
              leading={<span className={`dot ${connected ? "ok" : "wait"}`} aria-hidden="true" />}
            />
            {connected && status ? <Row title="Sürüm" meta={status.version} /> : null}
            {connected && status ? <Row title="Çalışan tur" meta={String(status.running_turns)} /> : null}
          </GroupedList>
          <GroupedList header="Motorlar" footer="Bu ayarlar config.toml içinden değiştirilir.">
            <Row
              title="Claude kotasını canlı oku"
              subtitle="config.toml içinden değiştirilir. Servis bu ayarın mevcut değerini bildirmiyor."
              meta="Bilinmiyor"
            />
            <Row
              title="Varsayılan motor"
              subtitle="config.toml içinden değiştirilir"
              meta={connected && status ? ENGINE_LABEL[status.default_engine] : "Bilinmiyor"}
            />
          </GroupedList>
          <GroupedList header="Proje kataloğunu içe aktar" footer="Katalog dosyasının yolunu gir. Yeni projeler eklenir, mevcut projeler güncellenir.">
            <form className="settings-import" onSubmit={importCatalog} aria-busy={catalog.pending}>
              <label htmlFor="catalog-path">Katalog dosyasının yolu</label>
              <input
                id="catalog-path"
                className="quota-input"
                type="text"
                value={path}
                spellCheck={false}
                autoComplete="off"
                disabled={catalog.pending}
                onChange={(event) => setPath(event.target.value)}
              />
              <CapsuleButton type="submit" disabled={!path.trim() || catalog.pending}>
                {catalog.pending ? "Katalog içe aktarılıyor…" : "Proje kataloğunu içe aktar"}
              </CapsuleButton>
              {catalog.pending ? <p role="status">Katalog işleniyor…</p> : null}
              {catalog.error ? <p className="quota-error" role="alert">{catalog.error}</p> : null}
              {catalog.report ? (
                <div role="status" aria-label="Katalog içe aktarma sonucu">
                  <p>Katalog içe aktarıldı.</p>
                  <dl className="settings-report">
                    <div><dt>Eklenen proje</dt><dd>{catalog.report.imported}</dd></div>
                    <div><dt>Güncellenen proje</dt><dd>{catalog.report.updated}</dd></div>
                  </dl>
                </div>
              ) : null}
            </form>
          </GroupedList>
          <GroupedList header="Projeleri içe aktar" footer="Sohbetler alınmaz; yalnız proje klasörleri ve her projenin geçmiş notu alınır. Gizli bilgi desenleri maskelenir.">
            <div className="settings-import" aria-busy={history.pending}>
              <p>Claude Code ve Codex oturumlarından proje klasörleri bulunur.</p>
              <CapsuleButton disabled={history.pending} onClick={() => void history.run(() => bridge.importProjects())}>
                {history.pending ? "Projeler içe aktarılıyor…" : "Projeleri içe aktar"}
              </CapsuleButton>
              {history.pending ? <p role="status">Oturumlar işleniyor; işlem bitince sonuç burada görünecek.</p> : null}
              {history.error ? <p className="quota-error" role="alert">{history.error}</p> : null}
              {history.report ? <HistoryImportReport report={history.report} /> : null}
            </div>
          </GroupedList>
        </>
      )}
    </Page>
  );
}

function HistoryImportReport({ report }: { report: ProjectImportReport }) {
  const failed = report.rows.filter((row) => row.note_status.startsWith("hata")).length;
  const attempted = report.rows.filter((row) => row.note_updated || row.note_status.startsWith("hata")).length;
  const message = failed === 0
    ? "Projeler içe aktarıldı."
    : failed === attempted
      ? "Projeler alındı ancak hiçbir not yazılamadı."
      : "Projeler içe aktarıldı; bazı notlar yazılamadı.";
  return (
    <div role="status" aria-label="Proje içe aktarma sonucu">
      <p>{message}</p>
      <dl className="settings-report">
        <div><dt>Proje</dt><dd>{report.rows.length}</dd></div>
        <div><dt>Yeni proje</dt><dd>{report.created}</dd></div>
        <div><dt>Güncellenen not</dt><dd>{report.notes_updated}</dd></div>
        <div><dt>Başarısız not</dt><dd>{failed}</dd></div>
        <div><dt>Eşleşmeyen oturum</dt><dd>{report.unmatched_sessions}</dd></div>
      </dl>
    </div>
  );
}

/** Çift tıklamayı engeller; önceki işlemin raporunu yeni işlem sırasında göstermez. */
function useImport<T>() {
  const [pending, setPending] = useState(false);
  const [report, setReport] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const busy = useRef(false);
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  async function run(action: () => Promise<T>) {
    if (busy.current) return;
    busy.current = true;
    setPending(true);
    setReport(null);
    setError(null);
    try {
      const value = await action();
      if (mounted.current) setReport(value);
    } catch (err) {
      if (mounted.current) setError(describeBridgeError(err));
    } finally {
      busy.current = false;
      if (mounted.current) setPending(false);
    }
  }

  return { pending, report, error, run };
}
