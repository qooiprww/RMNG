# Phase 0 spike findings — CT 113 (10.0.0.27), stock pegasis0/rmng, GNOME 48 headless (Ubuntu 26.04)

VALIDATED (apps = gnome-text-editor pid 950, alive throughout every experiment):
- RecordVirtual on a FRESH RemoteDesktop+ScreenCast session (record→start) adds a virtual
  monitor LIVE: 2 → 3 monitors, coexisting with the daemon's existing monitors. App survived.
- `org.gnome.Mutter.ScreenCast.Stream` HAS a `Stop` method (introspected: True).
- Stream.Stop removes the virtual monitor live: 3 → 2. App survived.
- gnome-shell never restarted; no app closed in any experiment. (No-app-loss = holds.)

BLOCKER for the plan's Task 3.1 mechanism:
- RecordVirtual on an ALREADY-STARTED session does NOT materialize a monitor (count stayed 3,
  stream created but no logical output). Re-Start errors "Already started". => cannot add a
  stream to the daemon's one live session. Per-stream diff on a single started session is out.

GetCurrentState text format (for parse_connectors):
  (uint32 <serial>, [ (('Meta-0','MetaVendor','Virtual remote monitor','0x...'),
     [('2560x1440@60.000',2560,1440,60.0,1.0,[scales],{'is-current':<true>,'is-preferred':<true>})],
     {monitor props}), (('Meta-1',...),[...],{...}) ],
   [ (2560,0,1.0,uint32 0,true,[('Meta-0',...)],@a{sv}{}), (0,0,1.0,0,false,[('Meta-1',...)],{}) ],
   {'layout-mode':<uint32 1>,...})
  => connector = 1st string of each monitor tuple's 1st element; current mode = the mode dict
     with 'is-current':<true>; logical-monitor position/primary in the 3rd top-level array.

Resolution change: a monitor's mode list only contains the mode RecordVirtual declared, so
ApplyMonitorsConfig cannot resize to an undeclared resolution => resize REQUIRES recreating the
stream (matches the diff design's stop+recreate for resized monitors).

CONCLUSION: pure per-stream Approach A (add/stop streams on one live session) is NOT supported.
Two viable live mechanisms that still keep apps open + no gnome-shell restart:
  A' make-before-break SESSION SWAP: build a new full session (all desired monitors) → switch
     capture/input to it → stop the old session. New monitors appear before old drop (no
     zero-output reshuffle). Cost: all monitors re-key once per switch (one brief IDR). Simpler.
  A'' per-monitor RD+SC session PAIRS: one session per monitor; add/stop/resize only the changed
     ones; unchanged monitors truly never blip. Cost: N sessions + input-routing rework. Complex.
