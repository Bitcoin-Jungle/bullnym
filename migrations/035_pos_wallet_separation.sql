ALTER TABLE donation_pages
    ADD COLUMN pos_ct_descriptor TEXT,
    ADD COLUMN pos_next_addr_idx INT NOT NULL DEFAULT 0;
