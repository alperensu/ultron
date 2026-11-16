import { EmptyState, NavBar, Sidebar, SidebarItem } from "./ui";
import { WindowControls } from "./ui/WindowControls";

/** Uygulama kabuğu. Gerçek ekranlar (Bugün, Sohbet) sonraki görevlerde gelir; servis köprüsü Task 8'de. */
export function Shell() {
  return (
    <div className="ui-shell">
      <Sidebar>
        <SidebarItem icon="sun" active>
          Bugün
        </SidebarItem>
      </Sidebar>
      <main className="ui-main">
        <NavBar title="Bugün" controls={<WindowControls />} />
        <div className="ui-stage">
          <div className="ui-stage__inner">
            <EmptyState
              title="Henüz bir şey yok"
              description="Projeler ve sohbetler yerel servise bağlanınca burada görünür. Bağlantı Aşama 2'nin sonraki adımında eklenir."
            />
          </div>
        </div>
      </main>
    </div>
  );
}
