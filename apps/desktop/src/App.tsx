import { useState } from "react";
import { KitchenSink } from "./KitchenSink";
import { Shell } from "./Shell";
import { selectBridge } from "./lib/selectBridge";
import { createAppStore, StoreProvider } from "./state/store";

export function App() {
  if (window.location.pathname === "/__ui") return <KitchenSink />;
  return <ShellWithStore />;
}

function ShellWithStore() {
  const [store] = useState(() => createAppStore(selectBridge()));
  return (
    <StoreProvider value={store}>
      <Shell />
    </StoreProvider>
  );
}
