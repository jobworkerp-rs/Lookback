/** A read-only value in a settings card. */
export function SettingRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="settings-row">
      <div className="settings-row-label">{label}</div>
      <input type="text" value={value} readOnly style={{ flex: 1, opacity: 0.7 }} />
    </div>
  );
}
