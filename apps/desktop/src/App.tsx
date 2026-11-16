import { useState } from "react";
import { KitchenSink } from "./KitchenSink";
import { Shell } from "./Shell";
import { selectBridge } from "./lib/selectBridge";
import { createAppStore, StoreProvider } from "./state/store";
import { ErrorBoundary } from "./ui/ErrorBoundary";

export function App() {
  return (
    <ErrorBoundary>
      {window.location.pathname === "/__ui" ? <KitchenSink /> : <ShellWithStore />}
    </ErrorBoundary>
  );
}

function ShellWithStore() {
  const [store] = useState(() => createAppStore(selectBridge()));
  return (
    <StoreProvider value={store}>
      <Shell />
    </StoreProvider>
  );
}
