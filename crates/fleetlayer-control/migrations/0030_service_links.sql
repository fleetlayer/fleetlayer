-- Links to web services the fleet uses but Fleetlayer doesn't run (a wiki, a CI server), added by the tenant's admins
-- on the dashboard's Services page. The name is also the policy's `service:<name>`, which decides who sees the link.
CREATE TABLE service_links (
    tenant TEXT NOT NULL,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    description TEXT,
    added_by TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, name)
);
