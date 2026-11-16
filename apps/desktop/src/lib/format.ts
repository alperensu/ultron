/** Kısa Türkçe göreli zaman: "az önce", "5 dk önce", "3 sa önce", "2 gün önce". */
export function relativeTime(iso: string, now: number = Date.now()): string {
  const diff = Math.max(0, now - Date.parse(iso));
  const minutes = Math.floor(diff / 60_000);
  if (minutes < 1) return "az önce";
  if (minutes < 60) return `${minutes} dk önce`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} sa önce`;
  return `${Math.floor(hours / 24)} gün önce`;
}

/** Milisaniyeyi "42 sn", "2 dk 14 sn" ya da "1 sa 5 dk" biçimine çevirir. */
export function formatDuration(ms: number): string {
  const total = Math.max(0, Math.round(ms / 1000));
  if (total < 60) return `${total} sn`;
  const minutes = Math.floor(total / 60);
  if (minutes < 60) return `${minutes} dk ${total % 60} sn`;
  return `${Math.floor(minutes / 60)} sa ${minutes % 60} dk`;
}

export const ENGINE_LABEL = { claude: "Claude", codex: "Codex", agy: "Antigravity" } as const;
