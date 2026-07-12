-- Add migration script here
CREATE TABLE instruction_access (
    id SERIAL PRIMARY KEY,
    instruction_id INT NOT NULL,
    position_id INT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (instruction_id) REFERENCES instructions(id),
    FOREIGN KEY (position_id) REFERENCES positions(id)
);