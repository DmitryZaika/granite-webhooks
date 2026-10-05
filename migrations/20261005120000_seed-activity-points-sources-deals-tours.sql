-- Points leaderboard: deals won, deal progress and tours count only from the
-- leaderboard launch, the same way calls and SMS already do.
--
-- activity_points_sources (20260928120000_create-phone-call-points.sql) holds
-- one "counts from" floor per source: nothing before counts_from scores, in
-- either period (This month and All time). This adds the three other sources:
--   deals_won      a won deal, dated by deals.updated_at (bumped when a deal is
--                  marked won; deals.updated_at has no ON UPDATE, so editing a
--                  deal's details does not move it)
--   deal_progress  each stage a deal entered, by deal_stage_history.entered_at
--   tours          tour / feature-task points, by user_points_ledger.awarded_at
--
-- 2026-10-04 04:00:00 UTC is midnight at the start of the launch day,
-- Sunday 2026-10-04, in US Eastern (EDT, UTC-4). counts_from is UTC, like
-- every date the app binds. The 'calls' and 'sms' rows are not touched: they
-- keep the moment the call/SMS scoring went live.
--
-- Data only, no schema change. INSERT IGNORE keeps a value already set, so a
-- re-run never undoes a later UPDATE.
--
-- Rollout: run this before deploying the SPA that reads these rows. The SPA
-- scores nothing for a source with no row (it never falls back to counting
-- all history), so the reverse order only shows zeros for these three sources
-- until this runs. The SPA running today ignores these rows, so running this
-- early is safe.
--
-- To move a floor later (UTC):
--   UPDATE activity_points_sources SET counts_from = '2026-10-04 04:00:00'
--    WHERE source IN ('deals_won', 'deal_progress', 'tours');

INSERT IGNORE INTO activity_points_sources (source, counts_from)
VALUES ('deals_won', '2026-10-04 04:00:00'),
       ('deal_progress', '2026-10-04 04:00:00'),
       ('tours', '2026-10-04 04:00:00');
