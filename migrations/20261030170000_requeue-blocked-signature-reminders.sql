-- The reminder job blocked every reminder: it waited for a second connection
-- from the Lambda's one-connection pool while its transaction held the only
-- one. Each still-open request gets its latest missed reminder now; run after
-- the fixed API is live.
UPDATE native_signature_jobs j
  INNER JOIN (
    SELECT request_id, company_id, MAX(due_at) AS due
      FROM native_signature_jobs
     WHERE kind = 'reminder' AND state = 'blocked'
     GROUP BY request_id, company_id
  ) latest ON latest.request_id = j.request_id AND latest.company_id = j.company_id AND latest.due = j.due_at
  INNER JOIN signature_requests sr ON sr.id = j.request_id AND sr.company_id = j.company_id
  INNER JOIN native_signature_documents nd ON nd.request_id = j.request_id AND nd.company_id = j.company_id
   SET j.state = 'pending', j.attempts = 0, j.due_at = UTC_TIMESTAMP(3)
 WHERE j.kind = 'reminder' AND j.state = 'blocked'
   AND sr.provider = 'native' AND sr.status IN ('sent', 'viewed')
   AND nd.revoked_at IS NULL
   AND (nd.expires_at IS NULL OR nd.expires_at > UTC_TIMESTAMP(3))
   AND JSON_EXTRACT(nd.settings_json, '$.reminders') = CAST('true' AS JSON)
   AND NOT EXISTS (SELECT 1 FROM native_signature_finalizations nf
                    WHERE nf.request_id = j.request_id AND nf.company_id = j.company_id);
