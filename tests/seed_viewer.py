"""Synthetic graph for the isolated viewer benchmark; never opens the real HOME."""
import sqlite3
import sys

path, count = sys.argv[1], int(sys.argv[2])
connection = sqlite3.connect(path)
with connection:
    connection.executescript("""
        INSERT OR IGNORE INTO agents(id,display_name,created_at) VALUES ('bench','bench',1);
        INSERT OR IGNORE INTO spaces(id,kind,label,created_by_agent_id,created_at)
            VALUES ('default','project','synthetic','bench',1);
        INSERT INTO sessions(id,space_id,agent_id,started_at) VALUES ('bench','default','bench',1);
    """)
    nodes = [(f'n{i:06}', 100 if i % 100 == 0 else 1) for i in range(count)]
    connection.executemany("""INSERT INTO nodes(id,space_id,created_by_agent_id,created_in_session_id,
        created_at,last_accessed,degree) VALUES (?,'default','bench','bench',1,1,?)""", nodes)
    connection.executemany("""INSERT INTO node_versions(node_id,space_id,revision,kind,label,body,source,
        valid_from,recorded_at,agent_id,session_id)
        VALUES (?,'default',1,'concept',?,'Synthetic benchmark only','synthetic',1,1,'bench','bench')""",
        [(node, node) for node, _ in nodes])
    edges = [(f'e{i:06}', f'n{i//100*100:06}', f'n{i:06}') for i in range(count) if i % 100]
    edges += [(f'b{i:06}', f'n{i-100:06}', f'n{i:06}') for i in range(100, count, 100)]
    connection.executemany("""INSERT INTO edges(id,space_id,from_node_id,to_node_id,created_by_agent_id,
        created_in_session_id,created_at,last_accessed) VALUES (?,'default',?,?,'bench','bench',1,1)""", edges)
    connection.executemany("""INSERT INTO edge_versions(edge_id,space_id,revision,relation,weight,confidence,
        valid_from,recorded_at,agent_id,session_id) VALUES (?,'default',1,'relates_to',1,0.9,1,1,'bench','bench')""",
        [(edge[0],) for edge in edges])
