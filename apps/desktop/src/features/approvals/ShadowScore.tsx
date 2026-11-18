/** Yerel modelin gölge P(güvenli) puanı. Yalnız bilgi: düğme, karar veya onay etkisi yoktur. */
export type ShadowScoreData = { p_safe: number } | null | undefined;

export function ShadowScore({ score }: { score?: ShadowScoreData }) {
  if (!score || !Number.isFinite(score.p_safe) || score.p_safe < 0 || score.p_safe > 1) return null;
  const percent = Math.round(score.p_safe * 100);
  return <p className="approval-card__meta">{`yerel model: %${percent} güvenli (gölge)`}</p>;
}
