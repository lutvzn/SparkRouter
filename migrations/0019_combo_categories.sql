-- Categories are routing policy, separate from dashboard-only provider tags.
ALTER TABLE combos ADD COLUMN category TEXT NOT NULL DEFAULT 'text'
    CHECK (category IN ('text','decision','image','video','music','tts','stt'));
UPDATE combos SET category = 'decision'
WHERE EXISTS (SELECT 1 FROM combo_steps s WHERE s.combo_id = combos.id)
AND NOT EXISTS (SELECT 1 FROM combo_steps s JOIN providers p ON p.id = s.provider_id
                WHERE s.combo_id = combos.id AND p.protocol <> 'systemone');

CREATE TABLE model_categories (
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model TEXT NOT NULL,
    category TEXT NOT NULL CHECK (category IN ('text','decision','image','video','music','tts','stt')),
    PRIMARY KEY (provider_id, model)
);
-- Keep existing configuration intact on other installations; routing revalidates
-- protocol/metadata and fails closed on incompatible legacy chains.
INSERT INTO model_categories (provider_id, model, category)
SELECT s.provider_id, s.model, MIN(c.category)
FROM combo_steps s JOIN combos c ON c.id = s.combo_id
GROUP BY s.provider_id, s.model HAVING COUNT(DISTINCT c.category) = 1;
