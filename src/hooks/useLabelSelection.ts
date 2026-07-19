import { useCallback, useMemo, useState } from "react";

/** Selected-label state shared by the Threads and Summaries label filter
 *  bars: toggle one / toggle many, plus the sorted array used as a query key
 *  (sorting keeps toggling labels in different orders from creating distinct
 *  cache entries for the same semantic selection). */
export function useLabelSelection(initial: string[] = []) {
  const [selectedLabels, setSelectedLabels] = useState<string[]>(initial);

  const toggleLabel = useCallback((label: string) => {
    setSelectedLabels((current) =>
      current.includes(label) ? current.filter((value) => value !== label) : [...current, label],
    );
  }, []);

  const toggleManyLabels = useCallback((labels: string[], turnOn: boolean) => {
    setSelectedLabels((current) => {
      const next = new Set(current);
      if (turnOn) for (const label of labels) next.add(label);
      else for (const label of labels) next.delete(label);
      return [...next];
    });
  }, []);

  const sortedLabels = useMemo(() => [...selectedLabels].sort(), [selectedLabels]);

  return { selectedLabels, setSelectedLabels, sortedLabels, toggleLabel, toggleManyLabels };
}
