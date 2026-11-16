import { useCallback, useEffect, useState } from "react";
import { Composer } from "../features/composer/Composer";
import { ChatPanel } from "../features/chat/ChatPanel";
import { EngineMenu } from "../features/chat/EngineMenu";
import { MessageList } from "../features/chat/MessageList";
import { useChat } from "../features/chat/useChat";
import { describeBridgeError } from "../lib/bridge";
import { useAppState, useStore } from "../state/store";
import { CapsuleButton, LargeTitle, NavBar, Sheet } from "../ui";
import { WindowControls } from "../ui/WindowControls";
import "../features/chat/chat.css";

export function Chat({ chatId }: { chatId: string }) {
  const { bridge } = useStore();
  const chat = useChat(bridge, chatId);
  const catalogChat = useAppState((state) => state.chats.find((value) => value.id === chatId));
  const projects = useAppState((state) => state.projects);
  const lastSent = useAppState((state) => state.lastSent?.chatId === chatId ? state.lastSent : null);
  const [progress, setProgress] = useState(0);
  const [cancelling, setCancelling] = useState(false);
  const [stopRequested, setStopRequested] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [panelOpen, setPanelOpen] = useState(false);
  const closePanel = useCallback(() => setPanelOpen(false), []);
  const title = catalogChat?.title ?? chat.detail?.chat.title ?? "Sohbet";
  const projectId = catalogChat?.project_id ?? chat.detail?.chat.project_id;
  const project = projects.find((value) => value.id === projectId);

  useEffect(() => {
    if (!chat.running) setStopRequested(false);
  }, [chat.running]);

  async function cancel() {
    if (cancelling) return;
    setCancelling(true);
    setActionError(null);
    try {
      await bridge.cancelTurn(chatId);
      setStopRequested(true);
    } catch (error) {
      setActionError(describeBridgeError(error));
    } finally {
      setCancelling(false);
    }
  }

  return (
    <main className="ui-main chat-screen" aria-label="Sohbet ekranı" data-event-count={chat.version}>
      <NavBar title={title} progress={progress} tools={
        <>
          <EngineMenu bridge={bridge} chatId={chatId} engine={chat.engine} disabled={chat.loading || chat.running || Boolean(chat.error)} />
          {chat.running ? <CapsuleButton variant="danger" disabled={cancelling || stopRequested} onClick={() => void cancel()}>{cancelling || stopRequested ? "Durduruluyor…" : "Durdur"}</CapsuleButton> : null}
          <button type="button" className="chat-details-toggle" onClick={() => setPanelOpen(true)}>Ayrıntılar</button>
        </>
      } controls={<WindowControls />} />
      <div className="chat-layout">
        <section className="chat-conversation" aria-label="Sohbet">
          <MessageList rows={chat.rows} version={chat.version} loading={chat.loading} onProgress={setProgress} sharedMessageId={lastSent ? chat.rows.find((row) => row.type === "user" && row.text === lastSent.text)?.id : undefined} header={
            <>
              <LargeTitle title={title} progress={progress} summary={`${project?.name ?? "Projesiz sohbet"}${chat.running ? " · çalışıyor" : ""}`} />
              {chat.loading ? <p className="chat-status" role="status">Sohbet yükleniyor…</p> : null}
              {chat.error ? <div className="chat-load-error" role="alert"><p>{chat.error}</p><CapsuleButton variant="text" onClick={chat.retry}>Yeniden dene</CapsuleButton></div> : null}
              {!chat.loading && !chat.error && chat.rows.length === 0 ? <p className="chat-status">Bu sohbet henüz boş. İlk mesajını yaz.</p> : null}
            </>
          } />
          {actionError ? <p className="chat-action-error" role="alert">{actionError}</p> : null}
          <Composer chatId={chatId} currentProjectId={projectId} disabled={chat.loading || chat.running || Boolean(chat.error)} />
        </section>
        <ChatPanel files={chat.files} />
      </div>
      <Sheet open={panelOpen} onClose={closePanel} title="Sohbet ayrıntıları" side="right">
        <ChatPanel files={chat.files} />
        <CapsuleButton variant="text" onClick={closePanel}>Kapat</CapsuleButton>
      </Sheet>
    </main>
  );
}
