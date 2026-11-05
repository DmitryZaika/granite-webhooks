-- Dema's sheet typed two December 2025 sales as 2026 (paid in early 2026).
UPDATE quote_commissions
SET signed_at = signed_at - INTERVAL 1 YEAR
WHERE company_id = 1
  AND quote_id IS NULL
  AND YEAR(signed_at) = 2026 AND MONTH(signed_at) = 12
  AND payroll_date < '2026-06-01'
  AND customer_label IN ('Michelle Orr', 'Terrica Jackson');
