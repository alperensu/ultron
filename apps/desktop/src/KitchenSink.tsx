import { useState } from "react";
import { ApprovalCard } from "./features/approvals/ApprovalCard";
import "./features/approvals/approvals.css";
import {
  CapsuleButton,
  Chip,
  EmptyState,
  GroupedList,
  Icon,
  LargeTitle,
  NavBar,
  QuotaRing,
  Row,
  Sheet,
  Sidebar,
  SidebarHeading,
  SidebarItem,
  Switch,
} from "./ui";

/** /__ui: tüm bileşenleri gösterir. Örnek veri yalnızca görsel kontrol içindir. */
export function KitchenSink() {
  const [on, setOn] = useState(true);
  const [sheet, setSheet] = useState<"bottom" | "right" | null>(null);
  const [approvalResolved, setApprovalResolved] = useState<string | null>(null);
  const [exampleCreated] = useState(() => new Date().toISOString());

  return (
    <div className="ui-shell" data-testid="kitchen-sink">
      <Sidebar footer={<SidebarItem icon="gear">Ayarlar</SidebarItem>}>
        <SidebarItem icon="plus">Yeni sohbet</SidebarItem>
        <SidebarItem icon="sun" active>
          Bugün
        </SidebarItem>
        <SidebarItem icon="bell" count={2}>
          Bildirimler
        </SidebarItem>
        <SidebarHeading>Projeler</SidebarHeading>
        <SidebarItem icon="folder" count={2}>
          Örnek proje
        </SidebarItem>
        <SidebarItem sub>Örnek sohbet</SidebarItem>
        <SidebarItem sub>Başka bir örnek sohbet</SidebarItem>
      </Sidebar>
      <main className="ui-main">
        <NavBar
          title="Bileşenler"
          tools={
            <>
              <span className="ui-navbar__tool">
                <QuotaRing percent={95} label="Örnek kota" />
              </span>
              <span className="ui-navbar__tool">
                <Icon name="bell" size={20} />
              </span>
            </>
          }
        />
        <div className="ui-stage">
          <div className="ui-stage__inner">
            <LargeTitle title="Bileşenler" summary="Tasarım sisteminin tüm parçaları. Veriler örnektir." />
            <GroupedList header="Örnek onay kartı">
              {approvalResolved ? <div className="approval-inbox"><p role="status">{approvalResolved}</p><CapsuleButton variant="text" onClick={() => setApprovalResolved(null)}>Örneği yeniden göster</CapsuleButton></div> : <div className="approval-inbox"><ApprovalCard approval={{
                id: "example-approval", chat_id: null, engine: "claude", category: "Delete", reason: "Örnek: dosya silmek için onay gerekiyor.",
                action_json: JSON.stringify({ Shell: { command: "rm example.txt; token=[MASKED]", cwd: "/example" } }), status: "pending",
                created_at: exampleCreated, resolved_at: null, resolved_by: null,
              }} onResolve={(_id, approve) => setApprovalResolved(approve ? "Örnek işlem onaylandı." : "Örnek işlem reddedildi.")} /></div>}
            </GroupedList>

            <GroupedList header="Satırlar" footer="Düğme satırı basınca küçülür; durağan satır küçülmez.">
              <Row index={0} title="Gezinti satırı" subtitle="Alt metin, 13 punto" onClick={() => {}} />
              <Row index={1} title="Meta ile satır" meta="02:14" onClick={() => {}} />
              <Row index={2} title="Durağan satır" subtitle="Tıklanamaz" meta={<Chip>Eko</Chip>} />
              <Row index={3} title="Vurgulu satır" tone="blue" onClick={() => {}} />
            </GroupedList>

            <GroupedList header="Anahtar ve düğmeler">
              <Row title="Bildirimler" trailing={<Switch checked={on} onChange={setOn} label="Bildirimler" />} />
              <div className="ks-row">
                <CapsuleButton>Onayla</CapsuleButton>
                <CapsuleButton variant="text">Vazgeç</CapsuleButton>
                <CapsuleButton variant="danger">Reddet</CapsuleButton>
                <CapsuleButton disabled>Devre dışı</CapsuleButton>
              </div>
            </GroupedList>

            <GroupedList header="Kota halkası">
              <div className="ks-row">
                <QuotaRing percent={42} label="Normal" size={44} />
                <QuotaRing percent={85} label="Uyarı" size={44} />
                <QuotaRing percent={100} label="Dolu" size={44} />
                <Chip>%42</Chip>
                <Chip tone="orange">%85</Chip>
                <Chip tone="red">%100</Chip>
              </div>
            </GroupedList>

            <GroupedList header="Sayfalar">
              <div className="ks-row">
                <CapsuleButton onClick={() => setSheet("bottom")}>Alt sayfa</CapsuleButton>
                <CapsuleButton variant="text" onClick={() => setSheet("right")}>
                  Yan sayfa
                </CapsuleButton>
              </div>
            </GroupedList>

            <GroupedList header="Boş durum">
              <EmptyState
                title="Henüz onay yok"
                description="Onaylar Aşama 3'te gelir; şimdilik bekleyen bir şey yok."
              />
            </GroupedList>
          </div>
        </div>
      </main>

      <Sheet open={sheet === "bottom"} onClose={() => setSheet(null)} title="Alt sayfa">
        <p>Odak sayfanın içinde kalır. Escape ya da arka plana tıklamak kapatır.</p>
        <div className="ks-row">
          <CapsuleButton onClick={() => setSheet(null)}>Tamam</CapsuleButton>
        </div>
      </Sheet>
      <Sheet open={sheet === "right"} onClose={() => setSheet(null)} title="Yan sayfa" side="right">
        <GroupedList>
          <Row title="Örnek satır" meta="42%" />
        </GroupedList>
      </Sheet>
    </div>
  );
}
