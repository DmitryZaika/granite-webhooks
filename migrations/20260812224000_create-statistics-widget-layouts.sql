CREATE TABLE statistics_widget_layouts (
    id INT AUTO_INCREMENT PRIMARY KEY,
    user_id INT NOT NULL,
    company_id INT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE KEY uq_statistics_widget_layouts_user_company (user_id, company_id),
    CONSTRAINT fk_statistics_widget_layouts_user
        FOREIGN KEY (user_id)
        REFERENCES users(id)
        ON DELETE CASCADE,
    CONSTRAINT fk_statistics_widget_layouts_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE
);

CREATE TABLE statistics_widget_layout_items (
    id INT AUTO_INCREMENT PRIMARY KEY,
    layout_id INT NOT NULL,
    widget_id VARCHAR(64) NOT NULL,
    position INT NOT NULL,
    show_chart TINYINT(1) NOT NULL DEFAULT 1,
    show_table TINYINT(1) NOT NULL DEFAULT 1,
    UNIQUE KEY uq_statistics_widget_layout_items_widget (layout_id, widget_id),
    INDEX idx_statistics_widget_layout_items_layout_position (layout_id, position),
    CONSTRAINT fk_statistics_widget_layout_items_layout
        FOREIGN KEY (layout_id)
        REFERENCES statistics_widget_layouts(id)
        ON DELETE CASCADE
);
