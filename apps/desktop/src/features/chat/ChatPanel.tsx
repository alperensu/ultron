import { useId, useState, type ReactNode } from "react";
import type { StoredEvent } from "../../lib/bridge";
import { field } from "./model";

/** Task 11 gerçek ajan listesini bu yuvaya bağlar; burada oturum/durum uydurulmaz. */
export function ChatPanel({ files, agents }: { files: StoredEvent[]; agents?: ReactNode }) {
  const [tab, setTab] = useState<"agents" | "changes">("agents");
  const [limit, setLimit] = useState(100);
  const id = useId();
  const tabs = ["agents", "changes"] as const;
  return (
    <aside className="chat-panel" aria-label="Sohbet ayrıntıları">
      <div className="chat-panel__tabs" role="tablist" aria-label="Ayrıntılar">
        {tabs.map((value) => (
          <button
            key={value}
            type="button"
            role="tab"
            id={`${id}-${value}`}
            aria-controls={`${id}-panel`}
            aria-selected={tab === value}
            tabIndex={tab === value ? 0 : -1}
            onClick={() => setTab(value)}
            onKeyDown={(event) => {
              if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
              event.preventDefault();
              const next = event.key === "Home" ? "agents" : event.key === "End" ? "changes" : tab === "agents" ? "changes" : "agents";
              setTab(next);
              document.getElementById(`${id}-${next}`)?.focus();
            }}
          >
            {value === "agents" ? "Ajanlar" : "Değişiklikler"}
          </button>
        ))}
      </div>
      <div id={`${id}-panel`} role="tabpanel" aria-labelledby={`${id}-${tab}`} className="chat-panel__body" tabIndex={0}>
        {tab === "agents" ? agents ?? <p className="chat-panel__empty">Ajan oturumları Aşama 2’nin 11. adımında burada listelenecek.</p> : files.length === 0 ? (
          <p className="chat-panel__empty">Henüz dosya değişikliği yok.</p>
        ) : (
          <>
            <p className="chat-panel__caption">{files.length} dosya değişikliği olayı</p>
            <ul className="chat-files">
              {files.slice(-limit).reverse().map((event) => (
                <li key={event.id}>
                  <span>{field(event, "path")}</span>
                  <small>{field(event, "change")}</small>
                </li>
              ))}
            </ul>
            {files.length > limit ? <button className="chat-panel__more" type="button" onClick={() => setLimit((value) => value + 100)}>Önceki 100 değişikliği göster</button> : null}
          </>
        )}
      </div>
    </aside>
  );
}
