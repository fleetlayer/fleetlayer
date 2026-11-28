-- How far each session has read the events it missed (`fleetlayer events`): the newest event ID it has been shown.
CREATE TABLE event_reads (
    tenant TEXT NOT NULL,
    target TEXT NOT NULL,
    last_id INTEGER NOT NULL,
    PRIMARY KEY (tenant, target)
);
