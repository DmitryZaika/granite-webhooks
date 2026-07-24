-- Add migration script here
CREATE TABLE marketing_refferal_sources (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
     company_id INT,
    name VARCHAR(255) NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP NULL,
    constraint fk_company_marketing_refferal_sources_id foreign key (company_id) references company(id)
);