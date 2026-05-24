-- 1. Create the folders table first
CREATE TABLE images_folders (
    id INT AUTO_INCREMENT PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    url VARCHAR(255) NOT NULL,
    company_id INT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP DEFAULT NULL,
    created_by VARCHAR(100) NOT NULL,
    FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
);

-- 2. Update your existing images table
ALTER TABLE images ADD COLUMN folder_id INT NULL;

-- 3. Add the foreign key constraint
ALTER TABLE images 
ADD CONSTRAINT fk_images_folder 
FOREIGN KEY (folder_id) REFERENCES images_folders(id) ON DELETE SET NULL;