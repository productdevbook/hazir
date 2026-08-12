CREATE TABLE site (
    id   bigserial PRIMARY KEY,
    name text NOT NULL
);

CREATE TABLE page (
    id      bigserial PRIMARY KEY,
    site_id bigint NOT NULL REFERENCES site (id) ON DELETE CASCADE,
    title   text   NOT NULL
);
