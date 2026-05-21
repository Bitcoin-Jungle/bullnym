-- Full Get Paid descriptor split.
--
-- users.ct_descriptor remains the Lightning Address descriptor (mobile path 75).
-- users.verification_npub is the public NIP-05 key; users.npub remains the
-- server-auth key used for signed Bullnym actions.
-- donation_pages.ct_descriptor is the Payment Page descriptor (mobile path 76)
-- with an independent address cursor.

ALTER TABLE users
    ADD COLUMN IF NOT EXISTS verification_npub TEXT;

UPDATE users
SET verification_npub = npub
WHERE verification_npub IS NULL;

ALTER TABLE users
    ALTER COLUMN verification_npub SET NOT NULL;

ALTER TABLE donation_pages
    ADD COLUMN IF NOT EXISTS ct_descriptor TEXT,
    ADD COLUMN IF NOT EXISTS next_addr_idx INT NOT NULL DEFAULT 0;
