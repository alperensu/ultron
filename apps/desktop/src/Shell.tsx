import { useEffect } from "react";
import { AppSidebar } from "./features/sidebar/AppSidebar";
import { Chat } from "./screens/Chat";
import { SettingsPlaceholder } from "./screens/SettingsPlaceholder";
import { Today } from "./screens/Today";
import { useRoute } from "./state/router";
import { useStore } from "./state/store";

/** Uygulama kabuğu: kenar çubuğu + rotaya göre ekran. Üstte sekme çubuğu yoktur. */
export function Shell() {
  const store = useStore();
  const route = useRoute();

  useEffect(() => store.start(), [store]);

  return (
    <div className="ui-shell">
      <AppSidebar />
      {route.name === "chat" ? (
        <Chat key={route.chatId} chatId={route.chatId} />
      ) : route.name === "settings" ? (
        <SettingsPlaceholder />
      ) : (
        <Today />
      )}
    </div>
  );
}
