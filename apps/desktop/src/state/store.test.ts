import { waitFor } from "@testing-library/react";
import { expect, it } from "vitest";
import { createFakeBridge, type Bridge } from "../lib/bridge";
import { createAppStore } from "./store";

it("eksik katalog listelerini başlangıçta ve sohbet yenilemede boş listeye çevirir", async () => {
  const fake = createFakeBridge();
  const bridge: Bridge = {
    ...fake,
    listProjects: async () => ({} as Awaited<ReturnType<Bridge["listProjects"]>>),
    listChats: async () => ({} as Awaited<ReturnType<Bridge["listChats"]>>),
  };
  const store = createAppStore(bridge);
  const stop = store.start();
  try {
    await waitFor(() => expect(store.getState().loaded).toBe(true));
    expect(store.getState().projects).toEqual([]);
    expect(store.getState().chats).toEqual([]);
    await store.createChat("Sentetik", null);
    expect(store.getState().chats).toEqual([]);
  } finally {
    stop();
    fake.dispose();
  }
});
