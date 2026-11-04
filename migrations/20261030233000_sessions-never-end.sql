-- Sign-ins no longer end by time (owner, 2026-10-30: "don't log out from any
-- device automatically"). Every web session that still signs in loses its
-- end date, so it lasts until sign-out or a password reset or change
-- (`auth::session::find_session_user`). Ended ones stay ended.
UPDATE sessions
   SET expiration_date = NULL
 WHERE is_deleted = 0
   AND expiration_date > CURRENT_TIMESTAMP
   AND (expiration_date <= CURRENT_TIMESTAMP + INTERVAL 60 DAY
        OR created_date > CURRENT_TIMESTAMP - INTERVAL 2 MONTH);
