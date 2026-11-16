import { useEffect, useState } from "react";
import { describeBridgeError, type Bridge, type ChatDetail, type StoredEvent } from "../../lib/bridge";
import { createChatTimeline, type ChatSnapshot } from "./model";

interface ChatState extends ChatSnapshot {
  detail: ChatDetail | null;
  loading: boolean;
  error: string | null;
}

const emptyTimeline = () => createChatTimeline().getSnapshot();

/** Abonelik geçmişten önce açılır. Geçmiş tamamlanana kadar canlı olaylar tamponda kalır. */
export function useChat(bridge: Bridge, chatId: string) {
  const [attempt, setAttempt] = useState(0);
  const [state, setState] = useState<ChatState>(() => ({
    ...emptyTimeline(), detail: null, loading: true, error: null,
  }));

  useEffect(() => {
    let stopped = false;
    let off: (() => void) | undefined;
    let frame = 0;
    let loading = true;
    let detail: ChatDetail | null = null;
    let pending: StoredEvent[] = [];
    let buffered: StoredEvent[] = [];
    let timeline = createChatTimeline();
    let idleThrough = -1;
    let checkingStatus = false;
    let checkedInitialStatus = false;

    // İptal edilen tur turn_completed üretmeyebilir. Sıfır, bütün sohbetlerin boşta
    // olduğunu kanıtlar; başka bir sohbet çalışırken bu sayaçtan kimlik çıkarılmaz.
    async function checkRunning() {
      const snapshot = timeline.getSnapshot();
      if (loading || stopped || checkingStatus || !snapshot.running || snapshot.version <= idleThrough) return;
      checkingStatus = true;
      try {
        const status = await bridge.status();
        if (!stopped && status.running_turns === 0 && timeline.getSnapshot().version === snapshot.version) {
          idleThrough = snapshot.version;
          setState((value) => value.version === snapshot.version ? { ...value, running: false } : value);
        }
      } catch {
        // Olay akışı çalışırken durum sorgusu hatası geçmişi silmez.
      } finally {
        checkingStatus = false;
      }
    }

    function publish() {
      if (stopped || frame) return;
      frame = requestAnimationFrame(() => {
        frame = 0;
        const snapshot = timeline.append(pending);
        pending = [];
        setState({ ...snapshot, running: snapshot.running && snapshot.version > idleThrough, detail, loading, error: null });
        if (!loading && !checkedInitialStatus) {
          checkedInitialStatus = true;
          void checkRunning();
        }
      });
    }

    async function load() {
      setState({ ...emptyTimeline(), detail: null, loading: true, error: null });
      try {
        off = await bridge.onEvent((event) => {
          if (stopped || event.chat_id !== chatId) return;
          if (loading) buffered.push(event);
          else {
            pending.push(event);
            publish();
          }
        });
        if (stopped) { off(); return; }
        detail = await bridge.getChat(chatId);
        if (stopped) return;
        timeline = createChatTimeline(detail.last_engine ?? detail.chat.engine);
        let after: number | null = null;
        while (!stopped) {
          const page = await bridge.eventsPage(chatId, after, 500);
          if (stopped) return;
          pending.push(...(page.events ?? []));
          publish();
          if (!page.has_more) break;
          if (page.next_after_seq === null || page.next_after_seq <= (after ?? 0)) {
            throw new Error("Sohbet geçmişi yüklenemedi: sayfa ilerlemiyor.");
          }
          after = page.next_after_seq;
        }
        if (stopped) return;
        // Bütün geçmişi canlı tamponla aynı karede sıralayıp tekilleştirir.
        pending.push(...buffered);
        buffered = [];
        loading = false;
        publish();
      } catch (error) {
        if (stopped) return;
        off?.();
        cancelAnimationFrame(frame);
        frame = 0;
        setState({ ...timeline.append(pending), detail, loading: false, error: describeBridgeError(error) });
      }
    }

    void load();
    const statusTimer = window.setInterval(() => void checkRunning(), 1000);
    return () => {
      stopped = true;
      off?.();
      cancelAnimationFrame(frame);
      window.clearInterval(statusTimer);
    };
  }, [bridge, chatId, attempt]);

  return { ...state, retry: () => setAttempt((value) => value + 1) };
}
