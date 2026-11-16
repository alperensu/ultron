import { motion } from "motion/react";
import { SENT_MESSAGE_LAYOUT_ID } from "../features/composer/Composer";
import { spring } from "../design/motion";
import { navigate } from "../state/router";
import { useAppState } from "../state/store";
import { CapsuleButton, EmptyState } from "../ui";
import { Page } from "./Page";

/**
 * Sohbet ekranı Task 10'da yapılır. Bu yer tutucu dürüsttür: sohbetin gerçek başlığını gösterir,
 * yeni gönderilen ilk mesaj tek kutudan buraya paylaşılan öğe animasyonuyla uçar.
 */
export function ChatPlaceholder({ chatId }: { chatId: string }) {
  const chat = useAppState((s) => s.chats.find((c) => c.id === chatId));
  const sentText = useAppState((s) => (s.lastSent?.chatId === chatId ? s.lastSent.text : null));
  const title = chat?.title ?? "Sohbet";
  return (
    <Page title={title}>
      {() => (
        <>
          {sentText ? (
            <motion.div layoutId={SENT_MESSAGE_LAYOUT_ID} transition={spring(0.5)} className="sent-bubble">
              {sentText}
            </motion.div>
          ) : null}
          <EmptyState
            title={title}
            description="Sohbet ekranı Aşama 2'nin sonraki adımında geliyor. Mesajın gönderildi ve sohbet kenar çubuğunda duruyor."
            action={
              <CapsuleButton variant="text" onClick={() => navigate("/")}>
                Bugün'e dön
              </CapsuleButton>
            }
          />
        </>
      )}
    </Page>
  );
}
