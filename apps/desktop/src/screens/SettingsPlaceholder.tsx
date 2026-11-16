import { EmptyState, LargeTitle } from "../ui";
import { Page } from "./Page";

/** Ayarlar Aşama 2'nin 12. adımında yapılır; o zamana dek dürüst boş durum. */
export function SettingsPlaceholder() {
  return (
    <Page title="Ayarlar">
      {(progress) => (
        <>
          <LargeTitle title="Ayarlar" progress={progress} />
          <EmptyState
            title="Ayarlar henüz yok"
            description="Kota paneli, içe aktarma ve talimatlar Aşama 2'nin 12. adımında burada olacak."
          />
        </>
      )}
    </Page>
  );
}
