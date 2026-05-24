ALTER TABLE edges ADD COLUMN confidence INTEGER NULL
    CHECK (confidence IS NULL OR (confidence BETWEEN 0 AND 100));
