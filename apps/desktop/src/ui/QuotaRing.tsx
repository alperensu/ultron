export type QuotaTone = "ok" | "warn" | "crit";

/** Eşikler: %70 altı yeşil, %70–89 turuncu, %90 ve üstü kırmızı. */
export function quotaTone(percent: number): QuotaTone {
  if (percent >= 90) return "crit";
  if (percent >= 70) return "warn";
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
