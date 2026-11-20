INSERT INTO users (id, login_name) VALUES ('alice', 'alice'), ('bob', 'bob'), ('carol', 'carol');
INSERT INTO teams (id) VALUES ('red');
INSERT INTO team_members (team_id, member_id) VALUES ('red', 'alice'), ('red', 'carol');
INSERT INTO products (id, name, price_cents) VALUES ('10000000-0000-4000-8000-000000000001', 'Moka pot', 3400);
INSERT INTO orders (id, owner_id, quantity) VALUES ('20000000-0000-4000-8000-000000000001', 'alice', 1), ('20000000-0000-4000-8000-000000000002', 'bob', 1);
INSERT INTO order_lines (order_id, line_no, owner_id, quantity) VALUES ('20000000-0000-4000-8000-000000000001', 1, 'alice', 1), ('20000000-0000-4000-8000-000000000001', 2, 'bob', 1), ('20000000-0000-4000-8000-000000000002', 1, 'bob', 1);
INSERT INTO photos (id, order_id, owner_id, content_id, team_id) VALUES ('30000000-0000-4000-8000-000000000001', '20000000-0000-4000-8000-000000000001', 'alice', '\x01', NULL), ('30000000-0000-4000-8000-000000000002', '20000000-0000-4000-8000-000000000002', 'bob', '\x02', 'red');
INSERT INTO team_notes (id, team_id, author_id, body) VALUES ('40000000-0000-4000-8000-000000000001', 'red', 'alice', 'kickoff');
