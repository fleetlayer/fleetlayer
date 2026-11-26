-- Session summaries, as the nodes' summaries plugins publish them (the nodes keep their own copies as Markdown files).
-- One row per session and period; a later record for the same period replaces it.
CREATE TABLE summaries (
    tenant TEXT NOT NULL DEFAULT 'default',
    node TEXT NOT NULL,
    session_id TEXT NOT NULL,
    period_kind TEXT NOT NULL,
    period_key TEXT NOT NULL,
    agent TEXT NOT NULL,
    activity_from INTEGER NOT NULL,
    activity_to INTEGER NOT NULL,
    written_at INTEGER NOT NULL,
    sentence TEXT NOT NULL,
    paragraph TEXT NOT NULL,
    page TEXT NOT NULL,
    tab TEXT,
    dir TEXT,
    origin TEXT NOT NULL,
    PRIMARY KEY (tenant, session_id, period_kind, period_key)
);
CREATE INDEX summaries_latest ON summaries (tenant, session_id, activity_to);

CREATE VIRTUAL TABLE summaries_fts USING fts5(sentence, paragraph, page, tab, content='summaries', content_rowid='rowid');
CREATE TRIGGER summaries_ai AFTER INSERT ON summaries BEGIN
    INSERT INTO summaries_fts (rowid, sentence, paragraph, page, tab) VALUES (new.rowid, new.sentence, new.paragraph, new.page, new.tab);
END;
CREATE TRIGGER summaries_ad AFTER DELETE ON summaries BEGIN
    INSERT INTO summaries_fts (summaries_fts, rowid, sentence, paragraph, page, tab) VALUES ('delete', old.rowid, old.sentence, old.paragraph, old.page, old.tab);
END;
CREATE TRIGGER summaries_au AFTER UPDATE ON summaries BEGIN
    INSERT INTO summaries_fts (summaries_fts, rowid, sentence, paragraph, page, tab) VALUES ('delete', old.rowid, old.sentence, old.paragraph, old.page, old.tab);
    INSERT INTO summaries_fts (rowid, sentence, paragraph, page, tab) VALUES (new.rowid, new.sentence, new.paragraph, new.page, new.tab);
END;

-- The last real activity per session, from every successful summary call (no_change answers too).
CREATE TABLE session_activity (
    tenant TEXT NOT NULL DEFAULT 'default',
    session_id TEXT NOT NULL,
    last_activity INTEGER NOT NULL,
    PRIMARY KEY (tenant, session_id)
);
