import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { describeBridgeError, type ChatInfo } from "../../lib/bridge";
import { chatPath, navigate, useRoute } from "../../state/router";
import { useAppState, useStore, type DaemonStatus } from "../../state/store";
import { Icon, Sidebar, SidebarItem } from "../../ui";
import { ProjectPicker } from "../composer/ProjectPicker";
import { ChatMenu, type ChatMenuAction } from "./ChatMenu";
import { ChatRow } from "./ChatRow";
import { SidebarSearch } from "./SidebarSearch";
import { buildSidebarRows, ROW_HEIGHT, visibleRange } from "./sidebarRows";
import "./sidebar.css";
import { useApprovals } from "../approvals/ApprovalProvider";

interface MenuState {
  chat: ChatInfo;
  x: number;
  y: number;
}

/** Kenar çubuğu: Ultron, Yeni sohbet, Ara, Bugün, Sabitlenenler, Projeler, Projesiz sohbetler; altta Ayarlar ve servis durumu. */
export function AppSidebar() {
  const store = useStore();
  const route = useRoute();
  const projects = useAppState((s) => s.projects);
  const chats = useAppState((s) => s.chats);
  const expanded = useAppState((s) => s.expandedProjects);
  const daemon = useAppState((s) => s.daemon);
  const approvals = useApprovals();

  const [searching, setSearching] = useState(false);
  const [query, setQuery] = useState("");
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [renamingId, setRenamingId] = useState<string | null>(null);
  const [moving, setMoving] = useState<ChatInfo | null>(null);
  const [error, setError] = useState<string | null>(null);

  const activeChatId = route.name === "chat" ? route.chatId : null;
  const rows = useMemo(() => buildSidebarRows(projects, chats, expanded), [projects, chats, expanded]);

  // Açık sohbetin projesi, kenar çubuğunda görünür olsun diye açılır.
  const activeProjectId = chats.find((c) => c.id === activeChatId)?.project_id ?? null;
  useEffect(() => {
    if (activeProjectId) store.expandProject(activeProjectId);
  }, [store, activeProjectId]);

  async function apply(chatId: string, patch: Parameters<typeof store.updateChat>[1]) {
    setError(null);
    try {
      await store.updateChat(chatId, patch);
    } catch (err) {
      setError(describeBridgeError(err));
    }
  }

  function onMenuAction(action: ChatMenuAction) {
    const chat = menu?.chat;
    setMenu(null);
    if (!chat) return;
    if (action === "rename") setRenamingId(chat.id);
    else if (action === "move") setMoving(chat);
    else if (action === "pin") void apply(chat.id, { pinned: !chat.pinned });
    else void apply(chat.id, { archived: true });
  }

  function leaveSearch() {
    setSearching(false);
    setQuery("");
  }

  return (
    <>
    <Sidebar
      className="app-sidebar"
      footer={
        <>
          <SidebarItem
            icon="gear"
            active={route.name === "settings"}
            onClick={() => navigate("/ayarlar")}
          >
            Ayarlar
          </SidebarItem>
          <ServiceStatus daemon={daemon} />
        </>
      }
    >
      <SidebarItem
        icon="plus"
        onClick={() => {
          leaveSearch();
          navigate("/");
          requestAnimationFrame(() => document.querySelector<HTMLTextAreaElement>(".composer textarea")?.focus());
        }}
      >
        Yeni sohbet
      </SidebarItem>
      <SidebarItem
        icon="search"
        active={searching}
        onClick={() => (searching ? leaveSearch() : setSearching(true))}
      >
        Ara
      </SidebarItem>
      <SidebarItem
        icon="sun"
        count={approvals.pending.length || undefined}
        active={route.name === "today" && !searching}
        onClick={() => {
          leaveSearch();
          navigate("/");
        }}
      >
        Bugün
      </SidebarItem>

      {error ? (
        <p className="sidebar-note tone-red" role="alert">
          {error}
        </p>
      ) : null}

      {searching ? (
        <SidebarSearch bridge={store.bridge} query={query} onQueryChange={setQuery} onClose={leaveSearch} />
      ) : (
        <VirtualList rowCount={rows.length} keyOf={(index) => rows[index]!.key}>
          {(index) => {
            const row = rows[index]!;
            switch (row.kind) {
              case "heading":
                return <div className="sidebar-heading">{row.label}</div>;
              case "empty":
                return <div className="sidebar-empty">{row.text}</div>;
              case "project":
                return (
                  <button
                    type="button"
                    className="project-row press"
                    aria-expanded={row.open}
                    onClick={() => store.toggleProject(row.project.id)}
                  >
                    <Icon name="chevron" size={11} className={`project-row__chev${row.open ? " is-open" : ""}`} />
                    <Icon name="folder" size={17} />
                    <span className="project-row__name">{row.project.name}</span>
                    <span className="project-row__count">{row.count}</span>
                  </button>
                );
              case "chat":
                return (
                  <ChatRow
                    chat={row.chat}
                    nested={row.nested}
                    active={row.chat.id === activeChatId}
                    renaming={renamingId === row.chat.id}
                    onOpen={() => navigate(chatPath(row.chat.id))}
                    onMenu={(x, y) => setMenu({ chat: row.chat, x, y })}
                    onRename={(title) => {
                      setRenamingId(null);
                      void apply(row.chat.id, { title });
                    }}
                    onCancelRename={() => setRenamingId(null)}
                  />
                );
            }
          }}
        </VirtualList>
      )}

    </Sidebar>
      {/* Menü ve sayfa kenar çubuğunun dışında çizilir: backdrop-filter, fixed konumu kendine bağlar. */}
      <ProjectPicker
        open={moving !== null}
        title="Projeye taşı"
        noneLabel="Projesiz"
        projects={projects}
        selectedId={moving?.project_id ?? null}
        onSelect={(projectId) => {
          const chat = moving;
          setMoving(null);
          if (chat) void apply(chat.id, { project_id: projectId });
        }}
        onClose={() => setMoving(null)}
      />
      {menu ? (
        <ChatMenu chat={menu.chat} x={menu.x} y={menu.y} onAction={onMenuAction} onClose={() => setMenu(null)} />
      ) : null}
    </>
  );
}

/** Sabit yükseklikli satırlar için küçük sanallaştırıcı: yalnız görünen aralık çizilir. */
function VirtualList({
  rowCount,
  keyOf,
  children,
}: {
  rowCount: number;
  keyOf: (index: number) => string;
  children: (index: number) => ReactNode;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [viewport, setViewport] = useState(600);

  useEffect(() => {
    const el = ref.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => setViewport(el.clientHeight || 600));
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  const { start, end } = visibleRange(rowCount, scrollTop, viewport);
  const items = [];
  for (let index = start; index < end; index++) {
    items.push(
      <div key={keyOf(index)} className="virtual-row" style={{ transform: `translateY(${index * ROW_HEIGHT}px)` }}>
        {children(index)}
      </div>,
    );
  }

  return (
    <div ref={ref} className="virtual-list" onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}>
      <div className="virtual-list__space" style={{ height: rowCount * ROW_HEIGHT }}>
        {items}
      </div>
    </div>
  );
}

function ServiceStatus({ daemon }: { daemon: DaemonStatus }) {
  const { dot, text } =
    daemon.state === "ready"
      ? {
          dot: daemon.runningTurns > 0 ? "run" : "ok",
          text: daemon.runningTurns > 0 ? `Servis hazır · ${daemon.runningTurns} tur çalışıyor` : "Servis hazır",
        }
      : daemon.state === "connecting"
        ? { dot: "wait", text: "Servise bağlanılıyor" }
        : { dot: "err", text: "Servis kapalı" };
  return (
    <div className="service-status" role="status" title={daemon.state === "unavailable" ? daemon.reason : undefined}>
      <span className={`dot ${dot}`} aria-hidden="true" />
      <span>{text}</span>
    </div>
  );
}
