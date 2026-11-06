-- Position access: the skills a company gives each position, beside its tabs
-- (Sell slabs, Work with transactions, Edit quotes, Plan the schedule).
-- Skill keys, comma-separated; NULL: the position follows the default skills
-- in api/crates/core/src/auth/skills.rs.
ALTER TABLE company_position_sections
  ADD COLUMN skills VARCHAR(500) NULL AFTER sections;
