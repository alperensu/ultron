import { useEffect, useState } from "react";
import { describeBridgeError, type Bridge, type SearchHit } from "../../lib/bridge";
import { chatPath, navigate } from "../../state/router";
import { useAppState } from "../../state/store";

const SEARCH_DEBOUNCE_MS = 250;

type SearchState =
  | { phase: "idle" }
  | { phase: "loading" }
  | { phase: "done"; hits: SearchHit[] }
  | { phase: "error"; message: string };

/** Ara: `historySearch` sonuçlarını kenar çubuğunda listeler; sonuca basınca sohbet açılır. */
export function SidebarSearch({
  bridge,
  query,
  onQueryChange,
  onClose,
}: {
  bridge: Bridge;
  query: string;
  onQueryChange: (query: string) => void;
  onClose: () => void;
}) {
  const [state, setState] = useState<SearchState>({ phase: "idle" });
  const chats = useAppState((s) => s.chats);
  const trimmed = query.trim();

  useEffect(() => {
    if (trimmed === "") {
      setState({ phase: "idle" });
      return;
    }
    let stale = false;
    setState({ phase: "loading" });
    const timer = window.setTimeout(() => {
      bridge.historySearch(trimmed, undefined, 20).then(
        ({ hits }) => !stale && setState({ phase: "done", hits: hits ?? [] }),
        (err: unknown) => !stale && setState({ phase: "error", message: describeBridgeError(err) }),
      );
    }, SEARCH_DEBOUNCE_MS);
    return () => {
      stale = true;
      window.clearTimeout(timer);
    };
  }, [bridge, trimmed]);

  const titleOf = (chatId: string) => chats.find((c) => c.id === chatId)?.title ?? "Sohbet";

  return (
    <div className="sidebar-search">
      <input
        type="search"
        className="sidebar-search__input"
        placeholder="Geçmişte ara"
        aria-label="Geçmişte ara"
        autoFocus
        value={query}
        onChange={(e) => onQueryChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Escape") onClose();
        }}
      />
      <div className="sidebar-search__results" aria-live="polite">
        {state.phase === "idle" ? <p className="sidebar-note">Mesajlarda aramak için yazmaya başla.</p> : null}
        {state.phase === "loading" ? <p className="sidebar-note">Aranıyor…</p> : null}
        {state.phase === "error" ? <p className="sidebar-note tone-red">{state.message}</p> : null}
        {state.phase === "done" && state.hits.length === 0 ? (
          <p className="sidebar-note">“{trimmed}” için sonuç yok.</p>
        ) : null}
        {state.phase === "done"
          ? state.hits.map((hit) => (
              <button
                key={hit.event_id}
                type="button"
                className="sidebar-hit press"
                onClick={() => {
                  navigate(chatPath(hit.chat_id));
                  onClose();
                }}
              >
                <span className="sidebar-hit__title">{titleOf(hit.chat_id)}</span>
                <span className="sidebar-hit__text">{hit.text}</span>
              </button>
            ))
          : null}
      </div>
    </div>
  );
}
