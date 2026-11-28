-- The cursor feed (`GET /v1/events?after=&channel=`) and pruning read events by channel.
CREATE INDEX events_channel ON events (tenant, channel, id);
CREATE INDEX events_at ON events (at);
