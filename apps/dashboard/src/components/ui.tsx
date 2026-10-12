import type { ReactNode } from "react";
import clsx from "clsx";

export function PageTitle({ title, subtitle, right }: { title: string; subtitle?: string; right?: ReactNode }) {
  return (
    <div className="flex items-center gap-3 flex-wrap">
      <h1 className="text-lg font-semibold">{title}</h1>
      {subtitle && <span className="text-sm text-gray-500">{subtitle}</span>}
      {right && <div className="ml-auto flex items-center gap-2">{right}</div>}
    </div>
  );
}

export function Card({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={clsx("rounded-lg border border-glass bg-surface/60 p-4", className)}>{children}</div>;
}

export function Stat({ label, value, tone }: { label: string; value: ReactNode; tone?: "pos" | "neg" }) {
  return (
    <div>
      <div className="text-[11px] uppercase tracking-wide text-gray-500">{label}</div>
      <div
        className={clsx(
          "font-mono text-lg",
          tone === "pos" && "text-emerald-light",
          tone === "neg" && "text-crimson-light",
        )}
      >
        {value}
      </div>
    </div>
  );
}

export function Pills({
  options,
  value,
  onChange,
}: {
  options: { key: string; label: string }[];
  value: string;
  onChange: (v: string) => void;
}) {
  return (
    <div className="flex rounded-md border border-glass overflow-hidden">
      {options.map((o) => (
        <button
          key={o.key}
          onClick={() => onChange(o.key)}
          className={clsx(
            "px-2.5 py-1 text-xs transition-colors",
            value === o.key ? "bg-violet/20 text-violet-light" : "text-gray-400 hover:bg-white/5",
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

export function Th({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <th className={clsx("text-left font-medium px-3 py-2 text-[11px] uppercase tracking-wide text-gray-400", className)}>
      {children}
    </th>
  );
}
export function Td({ children, className }: { children: ReactNode; className?: string }) {
  return <td className={clsx("px-3 py-2", className)}>{children}</td>;
}

export function Table({ head, children }: { head: ReactNode; children: ReactNode }) {
  return (
    <div className="overflow-auto rounded-lg border border-glass">
      <table className="w-full text-sm">
        <thead className="bg-surface sticky top-0">{head}</thead>
        <tbody>{children}</tbody>
      </table>
    </div>
  );
}

export function Msg({ children, tone }: { children: ReactNode; tone?: "error" }) {
  return (
    <div
      className={clsx(
        "rounded-lg border border-glass bg-surface/60 px-4 py-6 text-sm",
        tone === "error" ? "text-crimson-light" : "text-gray-400",
      )}
    >
      {children}
    </div>
  );
}

export function Input({
  value,
  onChange,
  className,
  type = "text",
  placeholder,
}: {
  value: string | number;
  onChange: (v: string) => void;
  className?: string;
  type?: string;
  placeholder?: string;
}) {
  return (
    <input
      type={type}
      value={value}
      placeholder={placeholder}
      onChange={(e) => onChange(e.target.value)}
      className={clsx(
        "bg-surface border border-glass rounded-md px-2 py-1 text-sm text-gray-100 focus:outline-none focus:border-violet/50",
        className,
      )}
    />
  );
}
