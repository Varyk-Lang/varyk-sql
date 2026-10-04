-- A migration whose SQL fails, for the test that a failed migration is an
-- Error naming its version.
select * from no_such_table;
