-- A stage someone moved the project back to by hand: the project stays at
-- most there (its calendar jobs don't move it on) until someone moves it
-- forward by hand. NULL: no cap.
ALTER TABLE projects ADD COLUMN stage_cap VARCHAR(32) NULL;
