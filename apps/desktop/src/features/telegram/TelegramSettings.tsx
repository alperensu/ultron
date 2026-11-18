import { useEffect, useRef, useState, type FormEvent } from "react";
import type {
  EngineId, TelegramErrorCode, TelegramSettings as Settings, TelegramStatusSnapshot, TelegramTestReport,
} from "../../lib/bridge";
import { ENGINE_LABEL } from "../../lib/format";
import { useAppState, useStore } from "../../state/store";
import { CapsuleButton, GroupedList, Row } from "../../ui";
import "./telegram.css";

const engines: EngineId[] = ["claude", "codex", "agy"];
const preferences = ["İlk tercih", "İkinci tercih", "Üçüncü tercih"];
const statusLabels: Record<TelegramStatusSnapshot["state"], string> = {
  connected: "Bağlı",
  missing_token: "Token yok",
  missing_allowed_users: "İzinli kullanıcı yok",
  error: "Hata",
};
const errorLabels: Record<TelegramErrorCode, string> = {
  not_verified: "Bağlantı henüz doğrulanmadı.",
  invalid_token: "Token geçersiz.",
  credential_unavailable: "Windows kimlik bilgisine erişilemedi.",
  network: "Telegram ağına erişilemedi.",
  timeout: "Telegram yanıtı zaman aşımına uğradı.",
  unauthorized: "Telegram yetkilendirmeyi reddetti.",
  poll_conflict: "Bot için başka bir yoklama açık.",
  rate_limited: "Telegram istek sınırına ulaşıldı.",
  provider_unavailable: "Telegram servisine erişilemedi.",
  invalid_response: "Telegram yanıtı geçersiz.",
  delivery_uncertain: "Teslim durumu doğrulanamadı.",
};

type Draft = Omit<Settings, "auto_continue_wait_minutes" | "fallback_order"> & {
  auto_continue_wait_minutes: string;
  fallback_order: (EngineId | "")[];
};

function asDraft(settings: Settings): Draft {
  return {
    ...settings,
    auto_continue_wait_minutes: String(settings.auto_continue_wait_minutes),
    fallback_order: engines.map((_, index) => settings.fallback_order[index] ?? ""),
  };
}

export function TelegramSettings() {
  const { bridge } = useStore();
  const daemon = useAppState((state) => state.daemon.state);
  const [snapshot, setSnapshot] = useState<TelegramStatusSnapshot | null>(null);
  const [edits, setEdits] = useState<Draft | null>(null);
  const [loading, setLoading] = useState(true);
  const [readFailed, setReadFailed] = useState(false);
  const [operation, setOperation] = useState<"save" | "test" | null>(null);
  const [feedback, setFeedback] = useState<{ text: string; error: boolean } | null>(null);
  const [report, setReport] = useState<TelegramTestReport | null>(null);
  const generation = useRef(0);
  const busy = useRef(false);
  const [reload, setReload] = useState(0);
  const previous = useRef({ daemon, bridge });

  useEffect(() => {
    if (previous.current.daemon !== daemon || previous.current.bridge !== bridge) {
      previous.current = { daemon, bridge };
      setEdits(null);
      setFeedback(null);
      setReport(null);
    }
    const current = ++generation.current;
    setSnapshot(null);
    setLoading(true);
    setReadFailed(false);
    void bridge.telegramStatus().then((value) => {
      if (current !== generation.current) return;
      setSnapshot(value);
    }, () => {
      if (current === generation.current) setReadFailed(true);
    }).finally(() => {
      if (current === generation.current) setLoading(false);
    });
    return () => { generation.current = current + 1; };
  }, [bridge, daemon, reload]);

  const draft = edits ?? (snapshot ? asDraft(snapshot.settings) : null);
  const minutes = Number(draft?.auto_continue_wait_minutes);
  const validMinutes = Number.isInteger(minutes) && minutes >= 1 && minutes <= 1440;
  const order = draft?.fallback_order.filter((engine): engine is EngineId => engine !== "") ?? [];
  const validOrder = new Set(order).size === order.length;
  const disabled = loading || !snapshot || operation !== null;
  const canTest = snapshot && snapshot.state !== "missing_token" && snapshot.state !== "missing_allowed_users";
  const unsaved = edits && snapshot && (
    edits.notify !== snapshot.settings.notify ||
    edits.auto_continue_enabled !== snapshot.settings.auto_continue_enabled ||
    minutes !== snapshot.settings.auto_continue_wait_minutes ||
    order.join(",") !== snapshot.settings.fallback_order.join(",")
  );

  async function run(action: "save" | "test") {
    if (busy.current || disabled || !draft) return;
    if (action === "save" && (!validMinutes || !validOrder)) return;
    if (action === "test" && !canTest) return;
    const current = generation.current;
    busy.current = true;
    setOperation(action);
    setFeedback(null);
    if (action === "test") setReport(null);
    try {
      if (action === "save") {
        const saved = await bridge.setTelegramSettings({
          notify: draft.notify,
          auto_continue_enabled: draft.auto_continue_enabled,
          auto_continue_wait_minutes: minutes,
          fallback_order: order,
        });
        if (current === generation.current) {
          setSnapshot((previous) => previous ? { ...previous, settings: saved } : previous);
          setEdits(null);
          setFeedback({ text: "Telegram ayarları kaydedildi.", error: false });
        }
      } else {
        const result = await bridge.telegramTest();
        if (current === generation.current) setReport(result);
      }
    } catch {
      if (current === generation.current) {
        setFeedback({
          text: action === "save"
            ? "Ayarların kaydedildiği doğrulanamadı. Durumu yenileyip tekrar kontrol et."
            : "Test mesajı için teslim raporu alınamadı; mesaj gönderilmiş olabilir. Telegram’ı kontrol edip daha sonra tekrar dene.",
          error: true,
        });
      }
    } finally {
      busy.current = false;
      setOperation(null);
      // Sonuç başka bir nesle ait olduğu için yok sayıldıysa form bayat kalmasın: bir kez daha oku.
      if (current !== generation.current) setReload((value) => value + 1);
    }
  }

  function change(patch: Partial<Draft>) {
    if (draft) setEdits({ ...draft, ...patch });
    setFeedback(null);
  }

  function save(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    void run("save");
  }

  const error = snapshot?.state === "error" ? snapshot.error : null;
  const errorText = error && Object.hasOwn(errorLabels, error) ? `${errorLabels[error]} (${error})` : undefined;

  return (
    <GroupedList header="Telegram">
      <div className="telegram-status" role="status" aria-busy={loading}>
        <Row
          title="Bağlantı durumu"
          meta={snapshot ? statusLabels[snapshot.state] : loading ? "Yükleniyor…" : "Hata"}
          subtitle={readFailed ? "Telegram durumu servisten okunamadı." : errorText}
          leading={<span className={`dot ${snapshot?.state === "connected" ? "ok" : snapshot?.state === "error" || readFailed ? "err" : "wait"}`} aria-hidden="true" />}
        />
      </div>
      <Row title="İzinli kullanıcı sayısı" meta={snapshot ? String(snapshot.allowed_user_count) : "—"} />
      <form className="telegram-form" onSubmit={save} aria-busy={loading || operation !== null}>
        <p className="telegram-help">Bot token’ını terminalde <code>ultron telegram set-token</code> komutuyla ayarla.</p>
        {draft && snapshot ? (
          <>
            <fieldset className="telegram-fields" disabled={disabled}>
              <label>
                Bildirim modu
                <select value={draft.notify} onChange={(event) => change({ notify: event.target.value === "away" ? "away" : "always" })} aria-describedby="telegram-notify-help">
                  <option value="always">Her zaman</option>
                  <option value="away">Masaüstü kapalıyken</option>
                </select>
              </label>
              <p id="telegram-notify-help" className="telegram-help">Masaüstü çalışırken arka plan bildirimlerini susturabilirsin. Telegram’daki komut ve sohbet yanıtları gelmeye devam eder.</p>
              <label className="telegram-check">
                <input type="checkbox" checked={draft.auto_continue_enabled} onChange={(event) => change({ auto_continue_enabled: event.target.checked })} />
                Kota dolduğunda otomatik devam et
              </label>
              <label>
                Bekleme süresi (dakika)
                <input type="number" min={1} max={1440} step={1} required value={draft.auto_continue_wait_minutes}
                  onChange={(event) => change({ auto_continue_wait_minutes: event.target.value })}
                  aria-invalid={!validMinutes} aria-describedby="telegram-wait-help" />
              </label>
              <p id="telegram-wait-help" className={validMinutes ? "telegram-help" : "telegram-error"}>1–1440 dakika arasında bir tam sayı gir. Varsayılan süre 45 dakika.</p>
              <fieldset className="telegram-fallback" aria-describedby="telegram-fallback-help">
                <legend>Yedek motor sırası</legend>
                <div className="telegram-fallback__choices">
                  {draft.fallback_order.map((engine, index) => (
                    <label key={index}>
                      {preferences[index]}
                      <select value={engine} aria-invalid={!validOrder} onChange={(event) => {
                        const selected = engines.find((item) => item === event.target.value) ?? "";
                        change({ fallback_order: draft.fallback_order.map((item, position) => position === index ? selected : item) });
                      }}>
                        <option value="">Seçilmedi</option>
                        {engines.map((item) => <option key={item} value={item}>{ENGINE_LABEL[item]}</option>)}
                      </select>
                    </label>
                  ))}
                </div>
                <p id="telegram-fallback-help" className={validOrder ? "telegram-help" : "telegram-error"}>
                  {validOrder ? "Başka bir motora otomatik geçişi kapatmak için tercihleri boş bırak." : "Her motoru bir kez seç."}
                </p>
              </fieldset>
            </fieldset>
            <div className="telegram-actions">
              <CapsuleButton type="submit" disabled={disabled || !validMinutes || !validOrder}>
                {operation === "save" ? "Kaydediliyor…" : "Telegram ayarlarını kaydet"}
              </CapsuleButton>
              <CapsuleButton variant="text" disabled={disabled || !canTest} onClick={() => void run("test")}>
                {operation === "test" ? "Test mesajı gönderiliyor…" : "Test mesajı gönder"}
              </CapsuleButton>
            </div>
            {unsaved ? <p className="telegram-help">Kaydedilmemiş değişiklikler var.</p> : null}
          </>
        ) : null}
        <CapsuleButton variant="text" disabled={loading || operation !== null} onClick={() => {
          setFeedback(null);
          setReload((value) => value + 1);
        }}>Durumu yenile</CapsuleButton>
      </form>
      {operation ? <p className="telegram-feedback telegram-help" role="status">{operation === "save" ? "Telegram ayarları kaydediliyor…" : "Telegram teslim raporu bekleniyor…"}</p> : null}
      {feedback ? <p className={`telegram-feedback${feedback.error ? " telegram-error" : ""}`} role={feedback.error ? "alert" : "status"}>{feedback.text}</p> : null}
      {report ? <TestReport report={report} /> : null}
    </GroupedList>
  );
}

function TestReport({ report }: { report: TelegramTestReport }) {
  let message = "Test mesajı hiçbir kullanıcıya teslim edilmedi.";
  if (report.attempted === 0) message = "Test mesajı için gönderim yapılmadı.";
  else if (report.delivered === report.attempted) message = "Test mesajı tüm hedeflere teslim edildi.";
  else if (report.delivered > 0) message = "Test mesajı kısmen teslim edildi.";
  else if (report.uncertain > 0) message = "Test mesajının teslimi doğrulanamadı.";

  return (
    <div role="status" aria-label="Telegram test sonucu" className="telegram-feedback">
      <p>{message}</p>
      <dl className="telegram-report">
        <div><dt>Denenen</dt><dd>{report.attempted}</dd></div>
        <div><dt>Teslim edilen</dt><dd>{report.delivered}</dd></div>
        <div><dt>Başarısız</dt><dd>{report.failed}</dd></div>
        <div><dt>Belirsiz</dt><dd>{report.uncertain}</dd></div>
      </dl>
    </div>
  );
}
