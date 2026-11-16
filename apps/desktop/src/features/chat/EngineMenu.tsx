import { useCallback, useState } from "react";
import { createPortal } from "react-dom";
import { describeBridgeError, type Bridge, type EngineId } from "../../lib/bridge";
import { ENGINE_LABEL } from "../../lib/format";
import { CapsuleButton, Sheet } from "../../ui";

export function EngineMenu({ bridge, chatId, engine, disabled }: {
  bridge: Bridge;
  chatId: string;
  engine: EngineId | null;
  disabled: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [selected, setSelected] = useState<EngineId | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const close = useCallback(() => {
    setOpen(false);
    setSelected(null);
    setError(null);
  }, []);

  async function confirm() {
    if (!selected || busy) return;
    setBusy(true);
    setError(null);
    try {
      await bridge.switchEngine(chatId, selected);
      setOpen(false);
      setSelected(null);
    } catch (err) {
      setError(describeBridgeError(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <button className="chat-engine press" type="button" aria-haspopup="dialog" disabled={disabled || busy} onClick={() => setOpen(true)}>
        Motor: {engine ? ENGINE_LABEL[engine] : "Bilinmiyor"} <span aria-hidden="true">▾</span>
      </button>
      {/* NavBar's backdrop-filter contains fixed descendants; mount the modal at viewport level. */}
      {createPortal(<Sheet open={open} onClose={close} title={selected ? "Motor değiştirilsin mi?" : "Motor seç"}>
        {selected ? (
          <div className="chat-engine__confirm">
            <p>Bu sohbetin bir sonraki mesajı {ENGINE_LABEL[selected]} ile sürdürülecek. Sohbet bağlamı yeni motora devredilecek.</p>
            {error ? <p role="alert">{error}</p> : null}
            <div className="chat-engine__actions">
              <CapsuleButton variant="text" disabled={busy} onClick={() => setSelected(null)}>Geri</CapsuleButton>
              <CapsuleButton disabled={busy} onClick={() => void confirm()}>{busy ? "Değiştiriliyor…" : "Motoru değiştir"}</CapsuleButton>
            </div>
          </div>
        ) : (
          <div className="chat-engine__options">
            {(["claude", "codex", "agy"] as const).map((id) => (
              <button type="button" key={id} disabled={id === engine} onClick={() => setSelected(id)}>
                {ENGINE_LABEL[id]}{id === engine ? " · şu anki motor" : ""}
              </button>
            ))}
          </div>
        )}
        <CapsuleButton variant="text" disabled={busy} onClick={close}>Vazgeç</CapsuleButton>
      </Sheet>, document.body)}
    </>
  );
}
