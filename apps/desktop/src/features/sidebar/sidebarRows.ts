import type { ChatInfo, ProjectInfo } from "../../lib/bridge";

/** Kenar çubuğu listesinin düz satırları; hepsi aynı yükseklikte olduğundan sanallaştırılır. */
export type SidebarRow =
  | { kind: "heading"; key: string; label: string }
  | { kind: "project"; key: string; project: ProjectInfo; count: number; open: boolean }
  | { kind: "chat"; key: string; chat: ChatInfo; nested: boolean }
  | { kind: "empty"; key: string; text: string };

export const ROW_HEIGHT = 32;

const byRecent = (a: ChatInfo, b: ChatInfo) => Date.parse(b.updated_at) - Date.parse(a.updated_at);

/**
 * Sıra (maket v2): Sabitlenenler, Projeler (açılır; altında sohbetler), Projesiz sohbetler.
 * Sabitlenmiş sohbet yalnız Sabitlenenler'de görünür; arşivlenmişler listede yoktur.
 */
export function buildSidebarRows(
  projects: ProjectInfo[],
  chats: ChatInfo[],
  expanded: readonly string[],
): SidebarRow[] {
  const live = chats.filter((c) => !c.archived).sort(byRecent);
  const pinned = live.filter((c) => c.pinned);
  const unpinned = live.filter((c) => !c.pinned);
  const knownProjects = new Set(projects.map((p) => p.id));
  const loose = unpinned.filter((c) => c.project_id === null || !knownProjects.has(c.project_id));

  const rows: SidebarRow[] = [];

  if (pinned.length > 0) {
    rows.push({ kind: "heading", key: "h-pinned", label: "Sabitlenenler" });
    for (const chat of pinned) rows.push({ kind: "chat", key: `p-${chat.id}`, chat, nested: false });
  }

  rows.push({ kind: "heading", key: "h-projects", label: "Projeler" });
  if (projects.length === 0) {
    rows.push({ kind: "empty", key: "e-projects", text: "Henüz proje yok" });
  }
  for (const project of projects) {
    const inProject = unpinned.filter((c) => c.project_id === project.id);
    const open = expanded.includes(project.id);
    rows.push({ kind: "project", key: `j-${project.id}`, project, count: inProject.length, open });
    if (!open) continue;
    if (inProject.length === 0) {
      rows.push({ kind: "empty", key: `e-${project.id}`, text: "Bu projede sohbet yok" });
    }
    for (const chat of inProject) rows.push({ kind: "chat", key: `c-${chat.id}`, chat, nested: true });
  }

  rows.push({ kind: "heading", key: "h-loose", label: "Projesiz sohbetler" });
  if (loose.length === 0) rows.push({ kind: "empty", key: "e-loose", text: "Projesiz sohbet yok" });
  for (const chat of loose) rows.push({ kind: "chat", key: `l-${chat.id}`, chat, nested: false });

  return rows;
}

/** Görünür aralık: kaydırma konumu ve görünüm yüksekliğinden, üstte/altta `overscan` satır fazlasıyla. */
export function visibleRange(
  total: number,
  scrollTop: number,
  viewport: number,
  overscan = 6,
): { start: number; end: number } {
  const start = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - overscan);
  const end = Math.min(total, Math.ceil((scrollTop + viewport) / ROW_HEIGHT) + overscan);
  return { start, end };
}
