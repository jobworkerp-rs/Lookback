export function SettingsCheckboxRow({
  id,
  checked,
  disabled = false,
  onCheckedChange,
  title,
  description,
}: {
  id: string;
  checked: boolean;
  disabled?: boolean;
  onCheckedChange: (checked: boolean) => void;
  title: string;
  description?: string;
}) {
  return (
    <label className="settings-checkbox-row" htmlFor={id}>
      <input
        id={id}
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(event) => onCheckedChange(event.target.checked)}
      />
      <span className="settings-checkbox-copy">
        <span className="settings-checkbox-title">{title}</span>
        {description && <span className="settings-checkbox-description">{description}</span>}
      </span>
    </label>
  );
}
