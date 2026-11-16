import { useEffect, useRef, useState, type KeyboardEvent, type MouseEvent } from "react";
import type { ChatInfo } from "../../lib/bridge";
import { Icon } from "../../ui";

/** Kenar çubuğundaki tek sohbet satırı: açma düğmesi, "…" menü düğmesi ve satır içi yeniden adlandırma. */
export function ChatRow({
  chat,
  nested,
  active,
  renaming,
  onOpen,
  onMenu,
  onRename,
  onCancelRename,
}: {
  chat: ChatInfo;
  nested: boolean;
  active: boolean;
  renaming: boolean;
  onOpen: () => void;
  onMenu: (x: number, y: number) => void;
  onRename: (title: string) => void;
  onCancelRename: () => void;
}) {
  const className = `chat-row${nested ? " is-nested" : ""}${active ? " is-active" : ""}`;

  if (renaming) {
    return (
      <div className={className}>
        <RenameInput initial={chat.title} onCommit={onRename} onCancel={onCancelRename} />
      </div>
    );
  }

  const openMenuAtPointer = (event: MouseEvent) => {
    event.preventDefault();
    onMenu(event.clientX, event.clientY);
  };

  return (
    <div className={className} onContextMenu={openMenuAtPointer}>
      <button
        type="button"
        className="chat-row__open press"
        aria-current={active ? "page" : undefined}
        onClick={onOpen}
      >
        {chat.pinned ? <Icon name="pin" size={13} className="chat-row__pin" /> : null}
        <span className="chat-row__title">{chat.title}</span>
      </button>
      <button
        type="button"
        className="chat-row__more"
        aria-label={`Sohbet menüsü: ${chat.title}`}
        aria-haspopup="menu"
        onClick={(event) => {
          const box = event.currentTarget.getBoundingClientRect();
          onMenu(box.left, box.bottom + 4);
        }}
      >
        <Icon name="more" size={16} />
      </button>
    </div>
  );
}

function RenameInput({
  initial,
  onCommit,
  onCancel,
}: {
  initial: string;
  onCommit: (title: string) => void;
  onCancel: () => void;
}) {
  const ref = useRef<HTMLInputElement>(null);
  const [value, setValue] = useState(initial);
  // Enter kaydeder ve satır kapanır; kapanırken tetiklenen blur ikinci kez kaydetmesin.
  const done = useRef(false);

  useEffect(() => {
    ref.current?.focus();
    ref.current?.select();
  }, []);

  function finish(save: boolean) {
    if (done.current) return;
    done.current = true;
    const title = value.trim();
    if (save && title !== "" && title !== initial) onCommit(title);
    else onCancel();
  }

  function onKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key === "Enter") finish(true);
    else if (event.key === "Escape") finish(false);
  }

  return (
    <input
      ref={ref}
      className="chat-row__rename"
      aria-label="Sohbet adı"
      value={value}
      onChange={(e) => setValue(e.target.value)}
      onKeyDown={onKeyDown}
      onBlur={() => finish(true)}
    />
  );
}
