-- ============================================================================
-- 033: POS terminal pairings and terminal-attributed invoices
-- ============================================================================

BEGIN;

CREATE TABLE pos_terminals (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    nym                 TEXT NOT NULL REFERENCES users(nym)
                        ON UPDATE CASCADE ON DELETE CASCADE,
    npub_owner          TEXT CHECK (npub_owner IS NULL OR npub_owner ~ '^[0-9a-f]{64}$'),
    label               TEXT CHECK (label IS NULL OR length(label) <= 100),
    token_hash          TEXT NOT NULL CHECK (token_hash ~ '^[0-9a-f]{64}$'),
    pairing_code_hash   TEXT CHECK (pairing_code_hash IS NULL OR pairing_code_hash ~ '^[0-9a-f]{64}$'),
    pairing_expires_at  TIMESTAMPTZ NOT NULL,
    claimed_at          TIMESTAMPTZ,
    last_seen_at        TIMESTAMPTZ,
    revoked_at          TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX pos_terminals_pairing_code_hash_unclaimed_idx
    ON pos_terminals (pairing_code_hash)
    WHERE claimed_at IS NULL;

CREATE INDEX pos_terminals_nym_token_active_idx
    ON pos_terminals (nym, token_hash)
    WHERE revoked_at IS NULL;

ALTER TABLE invoices
    ADD COLUMN terminal_id UUID REFERENCES pos_terminals(id) ON DELETE SET NULL,
    ADD COLUMN memo_public BOOLEAN NOT NULL DEFAULT FALSE;

ALTER TABLE invoices DROP CONSTRAINT invoices_checkout_no_metadata_chk;
ALTER TABLE invoices ADD CONSTRAINT invoices_checkout_no_metadata_chk
    CHECK (origin = 'wallet'
        OR terminal_id IS NOT NULL
        OR (memo               IS NULL
            AND recipient_label    IS NULL
            AND public_description IS NULL
            AND invoice_number     IS NULL));

ALTER TABLE invoices ADD CONSTRAINT invoices_terminal_metadata_chk
    CHECK (terminal_id IS NULL
        OR (recipient_label    IS NULL
            AND public_description IS NULL
            AND invoice_number     IS NULL));

ALTER TABLE invoices ADD CONSTRAINT invoices_memo_public_non_wallet_chk
    CHECK (memo_public = FALSE OR origin <> 'wallet');

CREATE INDEX invoices_pos_list_idx
    ON invoices (nym_owner, created_at DESC)
    WHERE terminal_id IS NOT NULL;

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'payservice') THEN
        GRANT SELECT, INSERT, UPDATE, DELETE
            ON pos_terminals
            TO payservice;

        GRANT SELECT, INSERT, UPDATE, DELETE
            ON invoices
            TO payservice;
    END IF;
END
$$;

COMMIT;
