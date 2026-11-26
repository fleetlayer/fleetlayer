-- Every connection to a tool shared by a node (a browser MCP server, say): the audit log.
CREATE TABLE tool_connections (
    tenant TEXT NOT NULL DEFAULT 'default',
    stream_id TEXT NOT NULL PRIMARY KEY,
    tool TEXT NOT NULL,
    server_node TEXT NOT NULL,
    caller TEXT NOT NULL,
    opened_at INTEGER NOT NULL,
    closed_at INTEGER,
    bytes_to_tool INTEGER NOT NULL DEFAULT 0,
    bytes_from_tool INTEGER NOT NULL DEFAULT 0,
    outcome TEXT
);
