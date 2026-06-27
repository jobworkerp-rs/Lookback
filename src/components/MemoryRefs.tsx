import { useTranslation } from "react-i18next";

/**
 * Numbered reference chips for a list of ids — shared by the Personality
 * signal/profile rows (memory_ids inside a known thread) and the Summaries
 * `source_memory_ids` / `source_thread_ids` reference arrays. Renders a
 * clickable button when `onOpen` is provided, else a static span so the
 * "this is a reference set" affordance survives even when the row hasn't
 * resolved a navigation target.
 */
export function MemoryRefs({
  ids,
  onOpen,
  titlePrefix = "memory",
}: {
  ids?: string[];
  onOpen?: (id: string) => void;
  /** Entity word used in the chip tooltip ("memory" / "thread"); a technical
   *  identifier, kept verbatim and interpolated into the localized tooltip. */
  titlePrefix?: string;
}) {
  const { t } = useTranslation();
  if (!ids || ids.length === 0) return null;
  return (
    <span className="signal-memory-refs">
      {ids.map((id, i) =>
        onOpen ? (
          <button
            key={id}
            type="button"
            className="signal-memory-chip"
            title={t("memoryRefs.open", { entity: titlePrefix, id })}
            onClick={() => onOpen(id)}
          >
            {i + 1}
          </button>
        ) : (
          <span
            key={id}
            className="signal-memory-chip"
            title={t("memoryRefs.label", { entity: titlePrefix, id })}
          >
            {i + 1}
          </span>
        ),
      )}
    </span>
  );
}
