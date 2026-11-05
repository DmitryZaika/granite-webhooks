-- Projects: Template Scheduled and Install Scheduled, between Awaiting
-- Template and Templated, and In Fabrication and Installed. A project moves
-- there on its own when its template or installation is on the calendar;
-- the stored stage is only where someone moved it by hand.
ALTER TABLE projects
  MODIFY COLUMN stage_code ENUM('new', 'template_scheduled', 'templated', 'fabrication',
                                'install_scheduled', 'installed', 'completed')
    NOT NULL DEFAULT 'new';
