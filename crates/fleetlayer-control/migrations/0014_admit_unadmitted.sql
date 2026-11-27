-- Since decisions moved to the owning tenant (docs/sso.md, step 1b), an agent no tenant admitted is refused. Every node
-- enrolled before then was admitted by 0013, and `node add` admits a new node's agents unless `--no-admit`; this
-- catches any node enrolled another way, so nothing that worked stops: a node no tenant admits, and that belongs to no
-- team or tenant, has its agents admitted to the deployment's first tenant. Recorded in that tenant's audit log.
CREATE TEMP TABLE unadmitted AS
    SELECT n.name AS name, (SELECT id FROM tenants ORDER BY created_at, id LIMIT 1) AS tenant
    FROM nodes n
    WHERE (n.owner IS NULL OR (n.owner NOT LIKE 'team:%' AND n.owner NOT LIKE 'tenant:%'))
      AND NOT EXISTS (
          SELECT 1 FROM member_agents m
          WHERE m.pattern = '*' OR m.pattern LIKE '%@' || n.name
      );

INSERT INTO member_agents (tenant, pattern, added_by, added_at)
    SELECT tenant, '*@' || name, 'migration', CAST(strftime('%s', 'now') AS INTEGER) * 1000 FROM unadmitted
    WHERE tenant IS NOT NULL;

INSERT INTO audit_outbox (tenant, at, decision, subject, verb, target, reason)
    SELECT tenant, CAST(strftime('%s', 'now') AS INTEGER) * 1000, 'changed', 'migration', 'agent.admit', '*@' || name,
           'admitted on upgrading, so it keeps working now that decisions need membership'
    FROM unadmitted WHERE tenant IS NOT NULL;

DROP TABLE unadmitted;
