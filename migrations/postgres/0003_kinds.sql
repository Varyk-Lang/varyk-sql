-- The columns whose types differ by database: a time, an id, and bytes,
-- and a timestamp with no zone. Each test keys its rows by its own name
-- in `test`.
create table kinds (
    test varchar(100) not null,
    id bigint not null,
    at timestamptz,
    uid uuid,
    data bytea,
    stamp timestamp,
    primary key (test, id)
);
