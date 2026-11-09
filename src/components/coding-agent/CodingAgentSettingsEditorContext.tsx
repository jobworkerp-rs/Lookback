import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createContext, useCallback, useContext, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  codingAgentGetSettings,
  codingAgentPrepareProfileHome,
  codingAgentUpdateSettings,
} from "@/api";
import type { CodingAgentSettings, CodingAgentSettingsResponse } from "@/types/api";
import type { CodingAgentSettingsValidationIssue } from "./settingsNormalization";
import { normalizeCodingAgentSettings, validateCodingAgentSettings } from "./settingsNormalization";

export const CODING_AGENT_SETTINGS_QUERY_KEY = ["coding-agent-settings"] as const;

export interface CodingAgentSettingsEditor {
  response: CodingAgentSettingsResponse | null;
  settings: CodingAgentSettings | null;
  revision: number | null;
  dirty: boolean;
  saving: boolean;
  setExternalSaving: (saving: boolean) => void;
  error: string | null;
  valid: boolean;
  validationIssues: CodingAgentSettingsValidationIssue[];
  setValid: (valid: boolean) => void;
  editing: boolean;
  setEditing: (editing: boolean) => void;
  update: (updater: (settings: CodingAgentSettings) => CodingAgentSettings) => void;
  replace: (response: CodingAgentSettingsResponse) => void;
  discard: () => void;
  save: () => Promise<CodingAgentSettingsResponse | null>;
  prepareHome: (profileId: string) => Promise<CodingAgentSettingsResponse | null>;
}

const CodingAgentSettingsEditorContext = createContext<CodingAgentSettingsEditor | null>(null);

export function useCodingAgentSettingsEditor(): CodingAgentSettingsEditor {
  const context = useContext(CodingAgentSettingsEditorContext);
  if (!context) {
    throw new Error(
      "useCodingAgentSettingsEditor must be used inside CodingAgentSettingsEditorProvider",
    );
  }
  return context;
}

export function CodingAgentSettingsEditorProvider({
  children,
  onChange,
}: {
  children: React.ReactNode;
  onChange?: (editor: CodingAgentSettingsEditor) => void;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const query = useQuery({
    queryKey: CODING_AGENT_SETTINGS_QUERY_KEY,
    queryFn: codingAgentGetSettings,
    retry: false,
  });
  const [draft, setDraft] = useState<CodingAgentSettings | null>(null);
  const [base, setBase] = useState<CodingAgentSettings | null>(null);
  const [saving, setSaving] = useState(false);
  const [externalSaving, setExternalSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [externalValid, setExternalValid] = useState(true);
  const [editing, setEditing] = useState(false);

  const response = query.data ?? null;
  useEffect(() => {
    if (!response) return;
    if (editing) return;
    const localDirty = Boolean(base && draft && JSON.stringify(draft) !== JSON.stringify(base));
    if (!base || !localDirty) {
      // The query is the source of truth until the first local edit. This
      // avoids an effect that can overwrite a draft while a refetch resolves.
      setBase(response.settings);
      setDraft(response.settings);
    }
  }, [base, draft, editing, response]);

  const settings = draft ?? response?.settings ?? null;
  const revision = base?.revision ?? response?.settings.revision ?? null;
  const dirty = Boolean(settings && base && JSON.stringify(settings) !== JSON.stringify(base));
  const busySaving = saving || externalSaving;
  const validationIssues = useMemo(
    () => (settings ? validateCodingAgentSettings(settings) : []),
    [settings],
  );
  const valid = externalValid && validationIssues.length === 0;

  const update = useCallback((updater: (settings: CodingAgentSettings) => CodingAgentSettings) => {
    setDraft((current) => (current ? updater(current) : current));
    setError(null);
  }, []);

  const replace = useCallback((next: CodingAgentSettingsResponse) => {
    setBase(next.settings);
    setDraft(next.settings);
    setError(null);
    setExternalValid(true);
  }, []);

  const discard = useCallback(() => {
    setDraft((current) => (base ? base : current));
    setError(null);
    setExternalValid(true);
    setEditing(false);
  }, [base]);

  const save = useCallback(async (): Promise<CodingAgentSettingsResponse | null> => {
    if (!settings || !base || !dirty || busySaving || !valid) return null;
    setSaving(true);
    setError(null);
    try {
      const next = await codingAgentUpdateSettings({
        expectedRevision: base.revision,
        settings: normalizeCodingAgentSettings(settings),
      });
      queryClient.setQueryData(CODING_AGENT_SETTINGS_QUERY_KEY, next);
      setBase(next.settings);
      setDraft(next.settings);
      setExternalValid(true);
      return next;
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : t("codingAgent.settings.saveError");
      setError(message);
      throw cause;
    } finally {
      setSaving(false);
    }
  }, [base, busySaving, dirty, queryClient, settings, t, valid]);

  const prepareHome = useCallback(
    async (profileId: string): Promise<CodingAgentSettingsResponse | null> => {
      if (!base || dirty || busySaving) return null;
      setSaving(true);
      setError(null);
      try {
        const next = await codingAgentPrepareProfileHome({
          expectedRevision: base.revision,
          profileId,
        });
        queryClient.setQueryData(CODING_AGENT_SETTINGS_QUERY_KEY, next);
        setBase(next.settings);
        setDraft(next.settings);
        return next;
      } catch (cause) {
        setError(
          cause instanceof Error ? cause.message : t("codingAgent.settings.prepareHomeError"),
        );
        throw cause;
      } finally {
        setSaving(false);
      }
    },
    [base, busySaving, dirty, queryClient, t],
  );

  const value = useMemo<CodingAgentSettingsEditor>(
    () => ({
      response,
      settings,
      revision,
      dirty,
      saving: busySaving,
      setExternalSaving,
      error,
      valid,
      validationIssues,
      setValid: setExternalValid,
      editing,
      setEditing,
      update,
      replace,
      discard,
      save,
      prepareHome,
    }),
    [
      dirty,
      discard,
      editing,
      error,
      prepareHome,
      replace,
      response,
      revision,
      save,
      busySaving,
      settings,
      update,
      valid,
      validationIssues,
    ],
  );

  useEffect(() => {
    onChange?.(value);
  }, [onChange, value]);

  return (
    <CodingAgentSettingsEditorContext.Provider value={value}>
      {children}
    </CodingAgentSettingsEditorContext.Provider>
  );
}
