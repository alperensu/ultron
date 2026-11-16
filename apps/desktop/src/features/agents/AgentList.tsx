import { motion } from "motion/react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { reducedMotion, spring } from "../../design/motion";
import { useRoute } from "../../state/router";
import { useStore } from "../../state/store";
import { useChat } from "../chat/useChat";
import { AgentDetail } from "./AgentDetail";
import { deriveAgents, elapsed, STATUS_LABEL } from "./model";
import { VirtualRows } from "./VirtualRows";
import "./agents.css";

/** ChatPanel'in mevcut yuvası sohbet kimliğini ortak yol yöneticisinden alır. */
export function ChatAgents() {
  const route = useRoute();
  return route.name === "chat" ? <AgentList key={route.chatId} chatId={route.chatId} /> : <p className="chat-panel__empty">Henüz ajan oturumu yok.</p>;
}

export function AgentList({ chatId }: { chatId: string }) {
  const { bridge } = useStore();
  const chat = useChat(bridge, chatId);
  const agents = useMemo(() => deriveAgents(chat.events, chatId, chat.engine), [chat.events, chatId, chat.engine]);
  const [selected, setSelected] = useState<string | null>(null);
  const [now, setNow] = useState(Date.now);
  const [stoppedAt, setStoppedAt] = useState<number | null>(null);
  const openedRow = useRef<HTMLButtonElement | null>(null);
  const panel = useRef<HTMLDivElement>(null);
  const close = useCallback(() => setSelected(null), []);
  const stopped = useCallback(() => setStoppedAt(Date.now()), []);
  const userTurns = useMemo(() => chat.events.filter((event) => event.kind === "user_text").length, [chat.events]);
  useEffect(() => {
    if (!chat.running) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [chat.running]);
  useEffect(() => { setStoppedAt(null); }, [userTurns]);
  useLayoutEffect(() => {
    if (selected === null) openedRow.current?.focus();
  }, [selected]);
  const session = agents.filter((agent) => !agent.parentId).at(-1);
  const selectedAgent = agents.find((agent) => agent.id === selected);
  useEffect(() => {
    if (!selected) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || !(event.target instanceof Node) || !panel.current?.contains(event.target)) return;
      // Pencere aşaması, dar ekrandaki mevcut ayrıntılar kabuğundan önce işler.
      event.preventDefault();
      event.stopPropagation();
      close();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [selected, close]);
  if (chat.loading) return <p className="chat-panel__empty" role="status">Ajan oturumları yükleniyor…</p>;
  if (chat.error) return <div className="agent-error" role="alert"><p>{chat.error}</p><button type="button" onClick={chat.retry}>Yeniden dene</button></div>;
  if (!agents.length) return <p className="chat-panel__empty">Henüz ajan oturumu yok. Bu sohbette bir motor oturumu başladığında burada görünür.</p>;
  return (
    <div className="agents-panel" ref={panel}>
      {/* Liste bağlı kalır: sanal kaydırma ve açılan satırın odağı korunur. */}
      <motion.div hidden={Boolean(selectedAgent)} initial={false}
        animate={reducedMotion() ? { opacity: selectedAgent ? 0 : 1 } : { opacity: selectedAgent ? 0 : 1, x: selectedAgent ? -20 : 0 }}
        transition={spring(0.24)}>
      <p className="agents-caption">
        {agents.filter((agent) => !agent.parentId).length} oturum · {agents.filter((agent) => agent.parentId).length} alt ajan
      </p>
      <VirtualRows rows={agents} height={100} label="Ajanlar" render={(agent) => {
        const isCurrent = agent === session || agent.parentId === session?.id;
        const status = isCurrent && stoppedAt && agent.status === "running" ? "Durdurma istendi"
          : isCurrent && !chat.running && agent.status === "running" ? "Tur çalışmıyor" : STATUS_LABEL[agent.status];
        const clock = stoppedAt && isCurrent ? stoppedAt
          : agent.status === "running" && !chat.running ? Date.parse(agent.events.at(-1)!.ts) : now;
        const dot = agent.status === "running" ? "run" : agent.status === "error" ? "err" : agent.status === "completed" ? "ok" : "wait";
        return (
          <button
            className={`agent-row press${agent.parentId ? " agent-row--child" : ""}`}
            type="button" onClick={(event) => { openedRow.current = event.currentTarget; setSelected(agent.id); }}
          >
            <span className={`dot ${dot}`} aria-hidden="true" />
            <span className="agent-row__body">
              <strong>{agent.title}</strong>
              <small>{agent.parentId ? "Claude · alt ajan · " : ""}{status}</small>
              <span title={agent.step}>{agent.step}</span>
            </span>
            <time>{elapsed(agent, clock)}</time>
          </button>
        );
      }} />
      </motion.div>
      {selectedAgent ? (
        <AgentDetail
          key={selectedAgent.id} agent={selectedAgent} bridge={bridge} chatId={chatId}
          currentEngine={chat.engine}
          current={selectedAgent.id === session?.id || selectedAgent.parentId === session?.id}
          running={chat.running && stoppedAt === null} now={stoppedAt ?? now}
          latestSeq={chat.events.at(-1)?.seq ?? 0}
          onClose={close} onStopped={stopped}
        />
      ) : null}
    </div>
  );
}
