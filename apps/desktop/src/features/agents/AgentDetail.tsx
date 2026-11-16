import { motion } from "motion/react";
import { useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore } from "react";
import { enterFrom, reducedMotion, spring } from "../../design/motion";
import { describeBridgeError, type Bridge, type EngineId } from "../../lib/bridge";
import { ENGINE_LABEL } from "../../lib/format";
import { CapsuleButton } from "../../ui";
import { agentMessageQueue } from "./messageQueue";
import { elapsed, eventSummary, STATUS_LABEL, type Agent } from "./model";
import { VirtualRows } from "./VirtualRows";
import { afterTurn } from "./afterTurn";

/** Modal olmayan, sohbetin sağ panelindeki oturum ayrıntısı. */
export function AgentDetail({ agent, bridge, chatId, running, currentEngine, current, now, latestSeq, onClose, onStopped }: {
  agent: Agent;
  bridge: Bridge;
  chatId: string;
  running: boolean;
  currentEngine: EngineId | null;
  current: boolean;
  now: number;
  latestSeq: number;
  onClose: () => void;
  onStopped: () => void;
}) {
  const queue = agentMessageQueue(bridge, chatId);
  const queued = useSyncExternalStore(queue.subscribe, queue.getSnapshot);
  const [busy, setBusy] = useState(false);
  const [stopRequested, setStopRequested] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);
  const [text, setText] = useState("");
  const back = useRef<HTMLButtonElement>(null);

  useEffect(() => queue.connect(), [queue]);
  useLayoutEffect(() => { back.current?.focus(); }, []);

  async function action(run: () => Promise<unknown>) {
    if (busy) return;
    setBusy(true);
    setError(null);
    try { await run(); }
    catch (cause) { setError(describeBridgeError(cause)); }
    finally { setBusy(false); }
  }

  async function stop() {
    await bridge.cancelTurn(chatId);
    setStopRequested(true);
    onStopped();
  }

  async function takeover(engine: EngineId) {
    // switch_engine çalışan turu reddeder; önce iptal gerçek köprüde tamamlanır.
    if (running && !stopRequested) await stop();
    await afterTurn(() => bridge.switchEngine(chatId, engine));
    setPicking(false);
  }

  const status = stopRequested ? "Durdurma istendi" : agent.status === "running" && !running ? "Tur çalışmıyor" : STATUS_LABEL[agent.status];
  const clock = agent.status === "running" && !running && !stopRequested ? Date.parse(agent.events.at(-1)!.ts) : now;
  return (
    <motion.section className="agent-detail" aria-label={agent.title}
      initial={reducedMotion() ? { opacity: 0 } : { opacity: 0, x: 20 }}
      animate={reducedMotion() ? { opacity: 1 } : { opacity: 1, x: 0 }} transition={spring(0.24)}>
        <button ref={back} type="button" className="agent-detail__back" onClick={onClose}>‹ Ajanlar</button>
        <h3 className="agent-detail__title">{agent.title}</h3>
        <div className="agent-detail__meta">
          <span>{agent.engine ? ENGINE_LABEL[agent.engine] : "Motor bilinmiyor"}{agent.parentId ? " · alt ajan" : ""}</span>
        </div>
        <div className="agent-detail__status" role="status">
          <span className={`dot ${agent.status === "running" ? "run" : agent.status === "error" ? "err" : "ok"}`} aria-hidden="true" />
          <span>{status}</span><time>{elapsed(agent, clock)}</time>
        </div>
        <VirtualRows key={agent.id} rows={agent.events} height={64} label="Ajan olayları" follow render={(event) => (
          <motion.div key={event.id} className="agent-event" initial={enterFrom(10)} animate={reducedMotion() ? { opacity: 1 } : { opacity: 1, y: 0 }} transition={spring(0.24)}>
            <time dateTime={event.ts}>{new Date(event.ts).toLocaleTimeString("tr-TR", { hour: "2-digit", minute: "2-digit", second: "2-digit" })}</time>
            <span title={eventSummary(event)}>{eventSummary(event)}</span>
          </motion.div>
        )} />
        <div className="agent-detail__controls">
          <p>İşlemler bu sohbetin tamamına uygulanır. Mesajlar sohbetin o anki motoruna iletilir.</p>
          <div className="agent-detail__actions">
            <CapsuleButton variant="danger" disabled={busy || !current || !running || stopRequested} onClick={() => void action(stop)}>{stopRequested ? "Durdurma istendi" : "Durdur"}</CapsuleButton>
            <CapsuleButton variant="text" disabled={busy || !current} aria-expanded={picking} onClick={() => setPicking((value) => !value)}>Devral</CapsuleButton>
          </div>
          {picking ? <fieldset className="agent-detail__engines" disabled={busy}><legend>Devralacak motor</legend>
            <div className="agent-detail__segments">
              {(["claude", "codex", "agy"] as const).map((engine) => <button type="button" key={engine} aria-pressed={engine === currentEngine} disabled={busy || engine === currentEngine} onClick={() => void action(() => takeover(engine))}>{ENGINE_LABEL[engine]}</button>)}
            </div>
          </fieldset> : null}
          <form className="agent-detail__form" onSubmit={(event) => {
            event.preventDefault();
            void action(async () => { await queue.enqueue(text, running && !stopRequested, latestSeq); setText(""); });
          }}>
            <label htmlFor={`agent-message-${agent.id}`}>Ajana mesaj yaz</label>
            <textarea id={`agent-message-${agent.id}`} value={text} maxLength={32000} onChange={(event) => setText(event.target.value)} rows={2} />
            <CapsuleButton type="submit" disabled={busy || !text.trim()}>Kuyruğa al</CapsuleButton>
          </form>
          {error ? <p className="agent-error" role="alert">{error}</p> : null}
          {busy ? <p role="status">İşlem sürüyor…</p> : null}
        </div>
        {queued.length ? <ul className="agent-queue" aria-label="Mesaj kuyruğu">{queued.map((row) => <li key={row.id}>
          <p>{row.text}</p><span>{row.status === "queued" ? "tur bitince iletilecek" : row.status === "sending" ? "İletiliyor…" : row.status === "sent" ? "Sohbete iletildi" : "İletilemedi"}</span>
          {row.error ? <p className="agent-error" role="alert">{row.error}</p> : null}
          {row.status === "failed" ? <CapsuleButton variant="text" disabled={busy} onClick={() => void action(async () => { await queue.enqueue(row.text, running, latestSeq); queue.remove(row.id); })}>Yeniden dene</CapsuleButton> : null}
          {row.status !== "sending" ? <CapsuleButton variant="text" onClick={() => queue.remove(row.id)}>{row.status === "queued" ? "Kuyruktan çıkar" : "Satırı kaldır"}</CapsuleButton> : null}
        </li>)}</ul> : null}
    </motion.section>
  );
}
