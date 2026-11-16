import { GroupedList, Icon, Row, Sheet } from "../../ui";
import type { ProjectInfo } from "../../lib/bridge";

/** Proje seçici (alt sayfa): tek kutu çipinden ve sohbet menüsünden "projeye taşı" için ortaktır. */
export function ProjectPicker({
  open,
  title = "Proje seç",
  projects,
  selectedId,
  noneLabel = "Projesiz",
  onSelect,
  onClose,
}: {
  open: boolean;
  title?: string;
  projects: ProjectInfo[];
  /** Seçili proje; `null` projesiz, `undefined` seçim yok. */
  selectedId?: string | null;
  noneLabel?: string;
  onSelect: (projectId: string | null) => void;
  onClose: () => void;
}) {
  const check = <Icon name="check" size={18} className="tone-blue" />;
  return (
    <Sheet open={open} onClose={onClose} title={title}>
      <GroupedList>
        <Row
          title={noneLabel}
          trailing={selectedId === null ? check : undefined}
          onClick={() => onSelect(null)}
        />
        {projects.map((project) => (
          <Row
            key={project.id}
            title={project.name}
            subtitle={project.description ?? undefined}
            trailing={selectedId === project.id ? check : undefined}
            onClick={() => onSelect(project.id)}
          />
        ))}
      </GroupedList>
      {projects.length === 0 ? (
        <p className="ui-section__footer">Henüz kayıtlı proje yok. Projeler, katalog içe aktarılınca burada görünür.</p>
      ) : null}
    </Sheet>
  );
}
