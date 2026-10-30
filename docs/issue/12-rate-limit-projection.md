# 12. No projection of the five-hour window

Kind: safety · Area: `stream`, `supervisor`, dashboard

## Problem

D6 reacts to `allowed_warning` and `rejected`, which arrive only when the account is already close to, or at,
its limit. Nothing looked at how fast the window was filling.

## Evidence

This single 6-worker run moved five-hour utilization from 0.29 to 0.76 in 26 minutes. At `max_active = 8`,
which the README suggested as a starting point, or with a second concurrent run, the window would have been
exhausted mid-run.

## Fix

- `stream` also parses the five-hour window's own reset time (`unifiedWindows.five_hour.resetsAt`).
- The supervisor keeps the last 15 minutes of (time, utilization) samples. The pure function
  `project_utilization` extrapolates the observed slope to the window's reset. It needs at least 3 minutes of
  samples.
- If the projection exceeds 100%, concurrency drops by one, at most once every two minutes, like
  `allowed_warning`. The AIMD increase (D6) still raises it again as turns succeed.
- The projection is in `RunStatus` and on the dashboard ("Usage 5h/7d: 52% → 118% at reset").
- The README no longer suggests starting at 8 on a single subscription without watching the projection.

## Verification

- `supervisor::tests::projection_*`: flat, rising, too-short history, past reset.
