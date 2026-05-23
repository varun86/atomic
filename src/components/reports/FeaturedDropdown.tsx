import { memo, useEffect, useRef, useState } from 'react';
import { Check, ChevronDown, Telescope } from 'lucide-react';
import { useReportsStore } from '../../stores/reports';
import { useFeaturedReportStore } from '../../stores/featuredReport';

/// Picker that lets the user choose which report fills the dashboard
/// widget's slot. Surfaces in the BriefingWidget's eyebrow row when
/// there's more than one report. With ≤1 report we render only the
/// label so the chevron doesn't promise an empty menu.
///
/// Wiring contract: this component does *not* fetch reports — that's
/// the responsibility of whichever view first lit them up (typically
/// the reports list). On a deep cold-start where only the dashboard
/// is rendered, we fetch lazily on first open. Once loaded, the
/// store keeps them around for the session.
interface FeaturedDropdownProps {
  /// The eyebrow text shown beside the chevron. Caller controls the
  /// label so this can read "BRIEFING · MAY 23" or just "DAILY
  /// BRIEFING" depending on context.
  label: string;
}

export const FeaturedDropdown = memo(function FeaturedDropdown({ label }: FeaturedDropdownProps) {
  const reports = useReportsStore(s => s.reports);
  const fetchAll = useReportsStore(s => s.fetchAll);
  const featuredId = useFeaturedReportStore(s => s.reportId);
  const setFeatured = useFeaturedReportStore(s => s.setFeatured);

  const [open, setOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  // Lazy-fetch when the user first opens the picker. Idempotent; if
  // reports are already loaded the store no-ops.
  useEffect(() => {
    if (open && reports.length === 0) {
      void fetchAll();
    }
  }, [open, reports.length, fetchAll]);

  // Close on outside click.
  useEffect(() => {
    if (!open) return;
    const onMouseDown = (e: MouseEvent) => {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener('mousedown', onMouseDown);
    return () => document.removeEventListener('mousedown', onMouseDown);
  }, [open]);

  // With zero or one report, there's nothing to pick — render the
  // label without an interactive chevron. (One-report case: the only
  // sensible action is "feature this one", and that's typically what
  // the seed already did.)
  if (reports.length <= 1) {
    return (
      <span className="text-[11px] font-medium uppercase tracking-[0.14em] text-[var(--color-text-tertiary)]">
        {label}
      </span>
    );
  }

  return (
    <div ref={containerRef} className="relative inline-block">
      <button
        type="button"
        onClick={() => setOpen(s => !s)}
        aria-haspopup="menu"
        aria-expanded={open}
        className="
          inline-flex items-center gap-1 text-[11px] font-medium uppercase tracking-[0.14em]
          text-[var(--color-text-tertiary)] hover:text-[var(--color-text-primary)] transition-colors
        "
      >
        {label}
        <ChevronDown className={`w-3 h-3 transition-transform ${open ? 'rotate-180' : ''}`} strokeWidth={2.5} />
      </button>

      {open && (
        <div
          role="menu"
          className="
            absolute left-0 mt-1.5 min-w-[220px] z-30
            bg-[var(--color-bg-card)] border border-[var(--color-border)]
            rounded-md shadow-xl py-1
            animate-in fade-in zoom-in-95 duration-100
          "
        >
          {reports.map((r) => {
            const isFeatured = featuredId === r.id;
            return (
              <button
                key={r.id}
                type="button"
                onClick={() => {
                  void setFeatured(r.id);
                  setOpen(false);
                }}
                role="menuitemradio"
                aria-checked={isFeatured}
                className={`
                  w-full px-3 py-1.5 text-left text-sm flex items-center gap-2
                  transition-colors
                  ${isFeatured ? 'text-[var(--color-accent-light)]' : 'text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]'}
                `}
              >
                <span className="w-3.5 flex-shrink-0">
                  {isFeatured ? <Check className="w-3.5 h-3.5" strokeWidth={2.5} /> : null}
                </span>
                <Telescope className="w-3.5 h-3.5 flex-shrink-0 text-[var(--color-text-tertiary)]" strokeWidth={2} />
                <span className="truncate">{r.name}</span>
              </button>
            );
          })}
          {featuredId !== null && (
            <>
              <div className="my-1 border-t border-[var(--color-border)]" />
              <button
                type="button"
                onClick={() => {
                  void setFeatured(null);
                  setOpen(false);
                }}
                role="menuitem"
                className="
                  w-full px-3 py-1.5 text-left text-sm
                  text-[var(--color-text-tertiary)] hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)]
                  transition-colors
                "
              >
                Unfeature
              </button>
            </>
          )}
        </div>
      )}
    </div>
  );
});
