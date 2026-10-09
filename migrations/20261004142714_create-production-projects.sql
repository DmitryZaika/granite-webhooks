-- Additive schema only. Apply through the normal reviewed deployment workflow.
CREATE TABLE projects (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    source_deal_id BIGINT UNSIGNED NOT NULL,
    customer_id INT NOT NULL,
    sale_id INT NULL,
    sales_rep_id INT NULL,
    owner_id INT NULL,
    title VARCHAR(255) NOT NULL,
    quoted_amount DECIMAL(10,2) NULL,
    project_address VARCHAR(255) NULL,
    stage_code ENUM('new','templated','fabrication','installed','completed') NOT NULL DEFAULT 'new',
    position INT NOT NULL DEFAULT 0,
    is_on_hold BOOLEAN NOT NULL DEFAULT FALSE,
    cancelled_at DATETIME NULL,
    templated_at DATETIME NULL,
    created_by INT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    version INT NOT NULL DEFAULT 1,
    archived_at DATETIME NULL,
    UNIQUE KEY uq_projects_source_deal (company_id, source_deal_id),
    UNIQUE KEY uq_projects_sale (company_id, sale_id),
    KEY idx_projects_board (company_id, archived_at, cancelled_at, stage_code, position, id),
    KEY idx_projects_owner (company_id, owner_id, archived_at, id),
    KEY idx_projects_rep (company_id, sales_rep_id, id),
    KEY idx_projects_customer (company_id, customer_id, id),
    CONSTRAINT fk_projects_company FOREIGN KEY (company_id) REFERENCES company(id),
    CONSTRAINT fk_projects_deal FOREIGN KEY (source_deal_id) REFERENCES deals(id),
    CONSTRAINT fk_projects_customer FOREIGN KEY (customer_id) REFERENCES customers(id),
    CONSTRAINT fk_projects_sale FOREIGN KEY (sale_id) REFERENCES sales(id),
    CONSTRAINT fk_projects_rep FOREIGN KEY (sales_rep_id) REFERENCES users(id),
    CONSTRAINT fk_projects_owner FOREIGN KEY (owner_id) REFERENCES users(id),
    CONSTRAINT fk_projects_creator FOREIGN KEY (created_by) REFERENCES users(id)
) ENGINE=InnoDB;

CREATE TABLE project_stage_history (
    id INT AUTO_INCREMENT PRIMARY KEY,
    project_id INT NOT NULL,
    company_id INT NOT NULL,
    from_stage VARCHAR(32) NULL,
    to_stage VARCHAR(32) NOT NULL,
    actor_id INT NOT NULL,
    reason TEXT NULL,
    source_action VARCHAR(64) NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    KEY idx_project_history (company_id, project_id, created_at, id),
    CONSTRAINT fk_project_history_project FOREIGN KEY (project_id) REFERENCES projects(id),
    CONSTRAINT fk_project_history_company FOREIGN KEY (company_id) REFERENCES company(id),
    CONSTRAINT fk_project_history_actor FOREIGN KEY (actor_id) REFERENCES users(id)
) ENGINE=InnoDB;

CREATE TABLE project_activities (
    id INT AUTO_INCREMENT PRIMARY KEY,
    project_id INT NOT NULL,
    company_id INT NOT NULL,
    name VARCHAR(255) NOT NULL,
    assigned_user_id INT NULL,
    deadline DATE NULL,
    priority ENUM('low','medium','high') NOT NULL DEFAULT 'medium',
    completed_at DATETIME NULL,
    created_by INT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at DATETIME NULL,
    KEY idx_project_tasks (company_id, project_id, deleted_at, completed_at, deadline),
    KEY idx_project_task_assignee (company_id, assigned_user_id, deadline),
    CONSTRAINT fk_project_activity_project FOREIGN KEY (project_id) REFERENCES projects(id),
    CONSTRAINT fk_project_activity_company FOREIGN KEY (company_id) REFERENCES company(id),
    CONSTRAINT fk_project_activity_assignee FOREIGN KEY (assigned_user_id) REFERENCES users(id),
    CONSTRAINT fk_project_activity_creator FOREIGN KEY (created_by) REFERENCES users(id)
) ENGINE=InnoDB;

CREATE TABLE project_notes (
    id INT AUTO_INCREMENT PRIMARY KEY,
    project_id INT NOT NULL,
    company_id INT NOT NULL,
    content TEXT NOT NULL,
    created_by INT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    deleted_at DATETIME NULL,
    KEY idx_project_notes (company_id, project_id, deleted_at, created_at),
    CONSTRAINT fk_project_note_project FOREIGN KEY (project_id) REFERENCES projects(id),
    CONSTRAINT fk_project_note_company FOREIGN KEY (company_id) REFERENCES company(id),
    CONSTRAINT fk_project_note_creator FOREIGN KEY (created_by) REFERENCES users(id)
) ENGINE=InnoDB;

CREATE TABLE project_files (
    id INT AUTO_INCREMENT PRIMARY KEY,
    project_id INT NOT NULL,
    company_id INT NOT NULL,
    name VARCHAR(255) NOT NULL,
    url VARCHAR(2048) NOT NULL,
    content_type VARCHAR(255) NOT NULL,
    created_by INT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at DATETIME NULL,
    KEY idx_project_files (company_id, project_id, deleted_at, created_at),
    CONSTRAINT fk_project_file_project FOREIGN KEY (project_id) REFERENCES projects(id),
    CONSTRAINT fk_project_file_company FOREIGN KEY (company_id) REFERENCES company(id),
    CONSTRAINT fk_project_file_creator FOREIGN KEY (created_by) REFERENCES users(id)
) ENGINE=InnoDB;

ALTER TABLE events
    ADD COLUMN project_id INT NULL,
    ADD KEY idx_events_project (project_id, deleted_date, start_date),
    ADD CONSTRAINT fk_events_project FOREIGN KEY (project_id) REFERENCES projects(id);
