-- Who messaged whom, and when: lets a session answer a session that messaged it recently, even where the rules
-- wouldn't let it start the conversation (an agent restricted to a short list can still reply to whoever wrote).
CREATE TABLE message_log (
    tenant TEXT NOT NULL DEFAULT 'default',
    from_subject TEXT NOT NULL,
    to_subject TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (tenant, from_subject, to_subject)
);
