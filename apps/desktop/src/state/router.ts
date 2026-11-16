import { useSyncExternalStore } from "react";

/** Küçük yol yöneticisi: "/" Bugün, "/chat/:id" Sohbet, "/ayarlar" Ayarlar. */
export type Route = { name: "today" } | { name: "chat"; chatId: string } | { name: "settings" };

export function parseRoute(pathname: string): Route {
  const chat = /^\/chat\/([^/]+)\/?$/.exec(pathname);
  if (chat) return { name: "chat", chatId: decodeURIComponent(chat[1]!) };
  if (pathname === "/ayarlar") return { name: "settings" };
  return { name: "today" };
}

const listeners = new Set<() => void>();

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  window.addEventListener("popstate", listener);
  return () => {
    listeners.delete(listener);
    window.removeEventListener("popstate", listener);
  };
}

export function navigate(path: string): void {
  if (window.location.pathname !== path) window.history.pushState(null, "", path);
  listeners.forEach((listener) => listener());
}

export function chatPath(chatId: string): string {
  return `/chat/${encodeURIComponent(chatId)}`;
}

export function useRoute(): Route {
  const pathname = useSyncExternalStore(subscribe, () => window.location.pathname);
  return parseRoute(pathname);
}
