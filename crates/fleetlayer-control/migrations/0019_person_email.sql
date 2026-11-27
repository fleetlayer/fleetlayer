-- A person's email address, set by an operator (`fleetlayer-control person email`). The sign-in service gives it to
-- services that ask for the `email` scope, as verified: whoever set it vouches for it (docs/plugins/signin.md).
ALTER TABLE people ADD COLUMN email TEXT;
