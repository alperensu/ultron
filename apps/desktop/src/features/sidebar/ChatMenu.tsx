import { useEffect, useRef } from "react";
import { createPortal } from "react-dom";
import type { ChatInfo } from "../../lib/bridge";

export type ChatMenuAction = "rename" | "pin" | "archive" | "move";

/**
 * Sohbet bağlam menüsü. `backdrop-filter` içeren kenar çubuğunda `position: fixed` bozulacağı için
 * gövdeye portal edilir. Escape ve dış tıklama kapatır; açılınca ilk öğeye odaklanır, kapanınca odağı geri verir.
 */
export function ChatMenu({
  chat,
  x,
  y,
  onAction,
  onClose,
}: {
  chat: ChatInfo;
  x: number;
  y: number;
  onAction: (action: ChatMenuAction) => void;
  onClose: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    ref.current?.querySelector<HTMLElement>("button")?.focus();

    const items = () => Array.from(ref.current?.querySelectorAll<HTMLElement>("button") ?? []);
    function onKey(event: KeyboardEvent) {
      if (event.key === "Escape") {
        event.stopPropagation();
        onClose();
      } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        const list = items();
        const at = list.indexOf(document.activeElement as HTMLElement);
        const step = event.key === "ArrowDown" ? 1 : -1;
        list[(at + step + list.length) % list.length]?.focus();
      } else if (event.key === "Tab") {
        event.preventDefault();
      }
    }
    function onPointerDown(event: PointerEvent) {
      if (!ref.current?.contains(event.target as Node)) onClose();
    }
    document.addEventListener("keydown", onKey, true);
    document.addEventListener("pointerdown", onPointerDown, true);
    return () => {
      document.removeEventListener("keydown", onKey, true);
      document.removeEventListener("pointerdown", onPointerDown, true);
      previous?.focus?.();
    };
  }, [onClose]);

  const left = Math.min(x, window.innerWidth - 220);
  const top = Math.min(y, window.innerHeight - 190);

  return createPortal(
    <div
      ref={ref}
      className="chat-menu"
      role="menu"
      aria-label={`${chat.title} için işlemler`}
      style={{ left, top }}
    >
      <button type="button" role="menuitem" onClick={() => onAction("rename")}>
        Yeniden adlandır
      </button>
      <button type="button" role="menuitem" onClick={() => onAction("pin")}>
        {chat.pinned ? "Sabitlemeyi kaldır" : "Sabitle"}
      </button>
      <button type="button" role="menuitem" onClick={() => onAction("move")}>
        Projeye taşı
      </button>
      <button type="button" role="menuitem" className="is-danger" onClick={() => onAction("archive")}>
        Arşivle
      </button>
    </div>,
    document.body,
  );
}
