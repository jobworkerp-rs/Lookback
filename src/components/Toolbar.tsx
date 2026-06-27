import type { ReactNode } from "react";

export interface ToolbarProps {
  title: string;
  subtitle?: string;
  actions?: ReactNode;
}

export function Toolbar({ title, subtitle, actions }: ToolbarProps) {
  return (
    <header className="toolbar">
      <h1 className="toolbar-title">{title}</h1>
      {subtitle && <span className="toolbar-subtitle">{subtitle}</span>}
      {actions && <div className="toolbar-actions">{actions}</div>}
    </header>
  );
}
