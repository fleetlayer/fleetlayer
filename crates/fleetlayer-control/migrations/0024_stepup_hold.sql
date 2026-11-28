-- A passkey step-up holds for the window length its page showed (the request's hold), so an approval can't open a
-- longer one than the person confirmed. Approve tokens' owners and step-ups' persons are person IDs from here on, so a
-- new person given an old handle inherits neither.
ALTER TABLE approval_stepups ADD COLUMN hold_ms INTEGER;
