-- The company's monthly sales goal, set by a Manager or Sales Manager and
-- shown to everyone on the QuickBooks sales widget's Company view.
ALTER TABLE company ADD COLUMN monthly_sales_goal DECIMAL(12, 2) NULL;
