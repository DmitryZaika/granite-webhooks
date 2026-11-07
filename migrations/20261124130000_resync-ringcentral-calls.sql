-- Re-read the last 100 days of every company's RingCentral call log, so the
-- saved calls (provider_calls) gain what the call-history job now keeps:
-- the call's result, every extension that may be its rep (the rep who
-- answered a queue call) and the other party's name. The job walks it back
-- to now a month a run and refreshes each call it already saved.
UPDATE provider_call_syncs
   SET synced_to = LEAST(synced_to, UTC_TIMESTAMP() - INTERVAL 100 DAY)
 WHERE provider = 'ringcentral';
