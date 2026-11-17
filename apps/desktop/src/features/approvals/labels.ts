import type { RiskCategory } from "../../lib/bridge";

export const categoryLabel: Record<RiskCategory, string> = {
  Delete: "Dosya silme", GitHistory: "Git geçmişi", Deploy: "Dağıtım", Finance: "Finans",
  Secrets: "Gizli bilgi", Install: "Kurulum", SystemConfig: "Sistem ayarı", Publish: "Yayınlama",
};
export function riskLabel(value: string): string {
  return Object.hasOwn(categoryLabel, value) ? categoryLabel[value as RiskCategory] : "Riskli işlem";
}
