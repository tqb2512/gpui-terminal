CREATE TABLE profiles (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    label TEXT NOT NULL,
    host TEXT NOT NULL,
    port INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
    username TEXT NOT NULL,
    auth TEXT NOT NULL CHECK (auth IN ('password', 'key')),
    key_path TEXT,
    last_connected INTEGER
);

CREATE TABLE host_trust (
    host TEXT NOT NULL,
    port INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
    revision INTEGER NOT NULL CHECK (typeof(revision) = 'integer' AND revision >= 0),
    public_key BLOB,
    PRIMARY KEY (host, port)
);
