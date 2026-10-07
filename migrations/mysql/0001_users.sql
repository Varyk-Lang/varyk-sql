-- The test schema. Portable across SQLite, Postgres, and MySQL: explicit
-- ids, no autoincrement. Each test keys its rows by its own name in `test`.
create table users (
    test varchar(100) not null,
    id bigint not null,
    name varchar(100) not null,
    age bigint,
    nick varchar(100),
    score double precision,
    active integer,
    done boolean,
    primary key (test, id)
);
