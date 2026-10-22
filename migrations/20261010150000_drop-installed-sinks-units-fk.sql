-- installed_sinks.sink_id holds a sink MODEL id (sink_type.id): every reader
-- joins it to sink_type, and the admin photos tab inserts the model id. The
-- original FK pointed it at sinks(id) (single units), which rejects photo
-- inserts unless a unit happens to share the model's id. Drop that FK only;
-- no data changes. Idempotent: does nothing where the FK is already gone.
SET @wrong_fk := (
  SELECT CONSTRAINT_NAME
  FROM information_schema.REFERENTIAL_CONSTRAINTS
  WHERE CONSTRAINT_SCHEMA = DATABASE()
    AND TABLE_NAME = 'installed_sinks'
    AND REFERENCED_TABLE_NAME = 'sinks'
  LIMIT 1
);
SET @drop_sql := IF(
  @wrong_fk IS NULL,
  'SELECT 1',
  CONCAT('ALTER TABLE installed_sinks DROP FOREIGN KEY `', @wrong_fk, '`')
);
PREPARE drop_wrong_fk FROM @drop_sql;
EXECUTE drop_wrong_fk;
DEALLOCATE PREPARE drop_wrong_fk;
