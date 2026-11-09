-- 0011: projects + linking tables (slice 8b).
--
-- A Project is a named container with a Kanban status column, color label,
-- notes, and two join tables: project_emails (message links, with auto-
-- participant extraction) and project_contacts (people links, with an origin
-- flag that distinguishes user-explicit 'manual' links from auto-inferred
-- 'auto' links created when a message is assigned to the project).
--
-- Migration id 10 belongs to the AI-queue PR (#49, parallel branch). This
-- migration id 11 must be applied AFTER id 10 or on a fresh DB where 10 has
-- not yet been applied (the MAX(id) watermark in run_migrations guarantees
-- this ordering: applying 11 first on a greenfield DB leaves the watermark
-- at 11, so 10 would be skipped on that DB -- both PRs must merge in id order).
CREATE TABLE projects (
    id          TEXT PRIMARY KEY,                -- uuid-v4 string
    name        TEXT NOT NULL,
    notes       TEXT NOT NULL DEFAULT '',        -- plain text v1
    status      TEXT NOT NULL DEFAULT 'active'
                CHECK (status IN ('active','waiting','someday','done')),
    color       TEXT,                            -- nullable hex or well-known name
    position    INTEGER NOT NULL DEFAULT 0,      -- sort within Kanban column
    archived    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,               -- epoch seconds
    updated_at  INTEGER NOT NULL,
    last_activity_at INTEGER NOT NULL
);
CREATE INDEX idx_projects_status_position ON projects(status, position);

CREATE TABLE project_emails (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    linked_at  INTEGER NOT NULL,
    PRIMARY KEY (project_id, message_id)
);
CREATE INDEX idx_project_emails_message ON project_emails(message_id);

CREATE TABLE project_contacts (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    contact_id INTEGER NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    origin     TEXT NOT NULL DEFAULT 'manual' CHECK (origin IN ('manual','auto')),
    linked_at  INTEGER NOT NULL,
    PRIMARY KEY (project_id, contact_id)
);
CREATE INDEX idx_project_contacts_contact ON project_contacts(contact_id);
