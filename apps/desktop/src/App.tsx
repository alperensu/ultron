import { KitchenSink } from "./KitchenSink";
import { Shell } from "./Shell";

export function App() {
  if (window.location.pathname === "/__ui") return <KitchenSink />;
  return <Shell />;
}
