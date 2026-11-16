export type QuotaTone = "ok" | "warn" | "crit";

/** Eşikler: %80 altı normal (yeşil), %80-99 uyarı (turuncu), %100 ve üstü kritik (kırmızı). */
export function quotaTone(percent: number): QuotaTone {
  if (percent >= 100) return "crit";
  if (percent >= 80) return "warn";
  return "ok";
}

const TONE_COLOR: Record<QuotaTone, string> = {
  ok: "var(--green)",
  warn: "var(--orange)",
  crit: "var(--red)",
};

const R = 11;
const C = 2 * Math.PI * R;

export function QuotaRing({
  percent,
  label,
  size = 28,
}: {
  percent: number;
  /** Erişilebilir ad, ör. "Codex haftalık". */
  label: string;
  size?: number;
}) {
  const clamped = Math.max(0, Math.min(100, Number.isFinite(percent) ? percent : 0));
  const tone = quotaTone(percent);
  return (
    <svg
      className="ui-ring"
      width={size}
      height={size}
      viewBox="0 0 28 28"
      role="img"
      aria-label={`${label}: %${Math.round(clamped)}`}
      data-tone={tone}
    >
      <circle cx="14" cy="14" r={R} fill="none" stroke="var(--fill)" strokeWidth="3" />
      <circle
        className="ui-ring__arc"
        cx="14"
        cy="14"
        r={R}
        fill="none"
        stroke={TONE_COLOR[tone]}
        strokeWidth="3"
        strokeLinecap="round"
        strokeDasharray={C}
        strokeDashoffset={C * (1 - clamped / 100)}
        transform="rotate(-90 14 14)"
      />
    </svg>
  );
}
